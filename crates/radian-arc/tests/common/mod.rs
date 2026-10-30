//! Shared helpers for the `radian-arc` integration tests.
//!
//! [`TestRepo`] is the small repository with a bare `origin` the gate tests use; [`Repo`] is
//! the bare remote, main clone and isolated home the `radian arc work` tests use. Every work
//! test runs a real `radian` binary and a real `radian acp` child, driven by the mock model
//! in `agent-provider-openai-compatible`'s `testing` feature.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use agent_provider_openai_compatible::testing::{MockServer, Reply};

/// A unique temporary directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-arc-{}-{label}-{n}", std::process::id()));
        Self::at(dir)
    }

    /// Wraps an already-chosen path and creates it, empty. A scratch name is unique only
    /// within one process (`<pid>-<n>`), so a directory left behind by an earlier run whose
    /// pid the OS handed out again is cleared first. A stale clone there made `git commit -m
    /// initial` exit 1 with "nothing to commit" on stdout and an empty stderr, which read as
    /// a mystery failure in an unrelated test.
    pub fn at(dir: PathBuf) -> Self {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!(
                "cannot clear the stale scratch directory {}: {error}",
                dir.display()
            ),
        }
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A wrapper or worker child can still have a directory here as its cwd when the test
        // ends; on macOS `rmdir` then fails with `EBUSY` even after every file is gone. Retry
        // briefly so the directory disappears once that child exits, instead of leaking empty
        // directories on every run.
        for _ in 0..100 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }
}

/// A repository with a bare `origin`, both in a temporary directory.
pub struct TestRepo {
    _dir: TempDir,
    pub repo: PathBuf,
    pub origin: PathBuf,
}

impl TestRepo {
    /// Creates `origin.git` (bare, `main`) and `repo` (one base commit on `main`, pushed).
    pub fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let origin = dir.path().join("origin.git");
        let repo = dir.path().join("repo");
        run(
            None,
            &["init", "--bare", "--initial-branch=main", path(&origin)],
        );
        run(None, &["init", "--initial-branch=main", path(&repo)]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        write(&repo.join("file.txt"), "base\n");
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["remote", "add", "origin", path(&origin)]);
        git(&repo, &["push", "-u", "origin", "main"]);
        Self {
            _dir: dir,
            repo,
            origin,
        }
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.repo, args)
    }

    /// Runs one git command in another directory with the test's identity.
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        git(dir, args)
    }

    /// The sha `origin` has for `refname`, read over the wire.
    pub fn ls_remote(&self, refname: &str) -> Option<String> {
        self.git(&["ls-remote", "origin", refname])
            .split_whitespace()
            .next()
            .map(str::to_owned)
    }

    /// Whether `ancestor` is an ancestor of `descendant`, without asserting success (the
    /// command's exit code is the answer).
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        let mut command = Command::new("git");
        command
            .current_dir(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["merge-base", "--is-ancestor", ancestor, descendant]);
        isolate_git(&mut command)
            .status()
            .expect("git runs")
            .success()
    }

    pub fn rev(&self, revision: &str) -> String {
        self.git(&["rev-parse", revision])
    }

    /// Checks out `branch` at `base`, commits one file and returns the commit id.
    pub fn commit_on(&self, branch: &str, base: &str, file: &str, content: &str) -> String {
        self.git(&["checkout", "-B", branch, base]);
        write(&self.repo.join(file), content);
        self.git(&["add", file]);
        self.git(&["commit", "-m", &format!("{branch}: {file}")]);
        self.rev("HEAD")
    }

    /// Commits one file on `main`, pushes it to the given remote and returns the commit id.
    pub fn advance_main_on(&self, remote: &str, file: &str, content: &str) -> String {
        self.git(&["checkout", "main"]);
        write(&self.repo.join(file), content);
        self.git(&["add", file]);
        self.git(&["commit", "-m", &format!("main: {file}")]);
        let sha = self.rev("HEAD");
        self.git(&["push", remote, "main"]);
        sha
    }

    /// Commits one file on `main`, pushes it to `origin` and returns the commit id.
    pub fn advance_main(&self, file: &str, content: &str) -> String {
        self.advance_main_on("origin", file, content)
    }

    pub fn push(&self, refspec: &str) {
        self.git(&["push", "origin", refspec]);
    }

    /// Renames `origin` to `remote` and leaves a breaking `origin` behind, so a command that
    /// still fetches the hardcoded name fails instead of quietly succeeding.
    pub fn rename_origin_to(&self, remote: &str) {
        self.git(&["remote", "rename", "origin", remote]);
        self.git(&[
            "remote",
            "add",
            "origin",
            path(&self.origin.with_extension("gone")),
        ]);
    }

    /// A second clone of the bare remote, for a commit this clone has not fetched or pushed.
    pub fn second_clone(&self, name: &str) -> PathBuf {
        let dir = self.repo.with_file_name(name);
        run(None, &["clone", path(&self.origin), path(&dir)]);
        dir
    }
}

pub fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

pub fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

/// Removes the git variables that would point a test's git command at another repository.
///
/// A worker's environment can carry `GIT_DIR`, `GIT_WORK_TREE` and the rest (ADR 0048
/// decisions 7 and 8: the wrapper's own record of the repository), and a harness that
/// inherited them would run its `git init`, `git config` and `git push` in the real checkout
/// instead of its temporary repository. `env_clear` also drops them; a helper that keeps the
/// process environment (for `PATH` and `HOME`) calls this instead.
pub fn isolate_git(command: &mut Command) -> &mut Command {
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    run(Some(dir), args)
}

fn run(dir: Option<&Path>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_AUTHOR_NAME", "Test");
    command.env("GIT_AUTHOR_EMAIL", "test@example.com");
    command.env("GIT_COMMITTER_NAME", "Test");
    command.env("GIT_COMMITTER_EMAIL", "test@example.com");
    let output = isolate_git(&mut command).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The git config of the checkout this test crate lives in: the real repository, which no test
/// may write. A test that runs a user command records it before the command and compares it
/// after, so a command that escaped its temporary repository fails the test instead of
/// changing the developer's checkout (and the sandbox's git control-file check).
///
/// Only the common `config` is guarded, because that is the file a stray `git config`, `git
/// push -u` or `git remote add` rewrites; the temporary repositories a test creates are not
/// this file. Content, not time, is what the check compares: a rewrite with the same bytes is
/// no change.
pub struct RealGitConfig {
    /// `None` when the crate is not checked out in a git repository: there is nothing to guard
    /// and every check is inert.
    path: Option<PathBuf>,
    /// The config as it was recorded.
    before: Vec<u8>,
}

impl RealGitConfig {
    /// Records the config of the repository holding `CARGO_MANIFEST_DIR`.
    pub fn record() -> Self {
        Self::of(Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    /// Records the config of the repository holding `dir`, asked of git with the environment
    /// isolated from this process: a `GIT_DIR` in the worker's environment must not decide
    /// which repository is the real one.
    pub fn of(dir: &Path) -> Self {
        let output = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .output();
        let text = output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|text| !text.is_empty());
        let Some(text) = text else {
            return Self {
                path: None,
                before: Vec::new(),
            };
        };
        let path = PathBuf::from(text);
        let common = if path.is_absolute() {
            path
        } else {
            dir.join(path)
        };
        match std::fs::canonicalize(&common) {
            Ok(common) => Self::at(&common),
            Err(_) => Self {
                path: None,
                before: Vec::new(),
            },
        }
    }

    /// Records the `config` of an explicit git common dir.
    pub fn at(common: &Path) -> Self {
        let path = common.join("config");
        let before = fs::read(&path).unwrap_or_default();
        Self {
            path: Some(path),
            before,
        }
    }

    /// The guarded file, when the crate is in a repository.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether the guarded config differs from the record. A change is restored before this
    /// returns, so the caller's failure leaves the developer's checkout as it was.
    pub fn changed(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let now = fs::read(path).unwrap_or_default();
        if now == self.before {
            return false;
        }
        let _ = fs::write(path, &self.before);
        true
    }

    /// Fails when the real repository's config changed since it was recorded, and puts the
    /// recorded content back first.
    pub fn assert_unchanged(&self) {
        if !self.changed() {
            return;
        }
        panic!(
            "a test changed the real repository's git config ({}); it was restored. A command \
             that runs git must stay inside its temporary repository.",
            self.path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        );
    }
}

/// A unique scratch directory under `$TMPDIR` — the `workspace` sandbox sets it to its own
/// writable temp directory, the only place a sandboxed test may create files. Its length does
/// not matter for the process control socket, because every scratch home turns that off (see
/// [`SCRATCH_HOME_CONFIG`]). Removed when the guard drops.
pub fn scratch(prefix: &str) -> TempDir {
    static DIRS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = DIRS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let name = format!("radian-arc-work-{prefix}-{}-{n}", std::process::id());
    TempDir::at(std::env::temp_dir().join(name))
}

/// The `config/config.toml` every scratch home gets before any process runs in it: the
/// process control port off. Every hosting process binds `$RADIAN_HOME/run/<pid>.sock`
/// (ADR 0043), so a scratch home under a long temp directory would push that path past
/// `sun_path` (104 bytes on macOS) and print `[control] disabled: path must be shorter than
/// SUN_LEN`. The arc wrapper and its worker are driven by stdin/stdout and never use the
/// port, so a test home turns it off rather than depend on the temp directory's length. That
/// is what lets [`scratch`] use `$TMPDIR` unconditionally, including the `workspace` sandbox's
/// own `/tmp`.
pub const SCRATCH_HOME_CONFIG: &str = "schema_version = 1\n\n[control]\nenabled = false\n";

#[cfg(test)]
mod scratch_tests {
    use super::*;

    /// A later change that drops the write, or brings the control socket back under the
    /// scratch home, fails here instead of only on a long temp directory.
    #[test]
    fn a_scratch_home_turns_the_control_port_off() {
        let repo = Repo::new("control-off");
        let path = repo.home.join("config").join("config.toml");
        let text = std::fs::read_to_string(&path).expect("the scratch home has a config");
        let config = radian_host::config::parse(&path, &text).expect("the config parses");
        assert_eq!(config.schema_version, Some(1));
        assert!(
            !config.control.enabled,
            "a scratch home must not bind the control socket: {text}"
        );
    }

    /// The harness drops the variables that would point a test's git at another repository: a
    /// `GIT_DIR` in the worker's environment must never reach a `git init`, a `git config` or
    /// a `git push` the tests run.
    #[test]
    fn the_git_isolation_helper_removes_the_repository_variables() {
        let mut command = Command::new("git");
        isolate_git(&mut command);
        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            assert!(
                removed.iter().any(|got| got == name),
                "`{name}` is not removed: {removed:?}"
            );
        }
    }

    /// A scratch name is unique only within one process (`<pid>-<n>`), so the OS can hand a
    /// pid to a later run that an earlier, interrupted run already used. That run's directory
    /// must not become the new fixture: a clone left with its `initial` commit made
    /// `git commit -m initial` exit 1 with "nothing to commit" and an empty stderr.
    #[test]
    fn a_reused_scratch_name_starts_empty() {
        let path =
            std::env::temp_dir().join(format!("radian-arc-work-reused-{}-0", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the earlier run's directory");
        std::fs::write(path.join("leftover"), "from an earlier run").expect("the leftover");

        let dir = TempDir::at(path);

        assert!(
            !dir.join("leftover").exists(),
            "a reused scratch directory must start empty: {}",
            dir.path().display()
        );
    }
}

fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    exe.parent()
        .and_then(Path::parent)
        .expect("the test binary lives in <target>/<profile>/deps")
        .to_path_buf()
}

fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if let Ok(modified) = meta.modified() {
                newest = Some(match newest {
                    Some(previous) => previous.max(modified),
                    None => modified,
                });
            }
        }
    }
    newest
}

fn newest_source_mtime() -> Option<std::time::SystemTime> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives in <root>/crates/radian-arc");
    let mut newest: Option<std::time::SystemTime> = None;
    for base in ["crates", "third_party"] {
        let Ok(entries) = std::fs::read_dir(root.join(base)) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            for candidate in [dir.join("src"), dir.join("build.rs")] {
                let found = if candidate.is_dir() {
                    newest_mtime(&candidate)
                } else {
                    std::fs::metadata(&candidate)
                        .and_then(|meta| meta.modified())
                        .ok()
                };
                if let Some(found) = found {
                    newest = Some(match newest {
                        Some(previous) => previous.max(found),
                        None => found,
                    });
                }
            }
        }
    }
    newest
}

/// The `radian` binary, refusing a missing or stale one: `cargo nextest -p radian-arc` does
/// not rebuild `radian-tui`, so a stale binary would silently exercise the previous build.
pub fn radian_bin() -> PathBuf {
    let path = target_dir().join("radian");
    assert!(
        path.exists(),
        "radian is not built; run: cargo build -p radian-tui --bin radian"
    );
    let binary_mtime = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .expect("the radian binary's mtime");
    if let Some(newest) = newest_source_mtime() {
        assert!(
            newest <= binary_mtime,
            "radian is older than its sources: run `cargo build -p radian-tui --bin radian`"
        );
    }
    path
}

/// A catalog URL that fails at once on every OS (ADR 0018's `RADIAN_CATALOG_URL` seam), so
/// no run reaches models.dev. One 503 sink per test process.
pub fn fast_catalog_url() -> String {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the catalog sink");
        let addr = listener.local_addr().expect("the sink's address");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                use std::io::Read;
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        format!("http://{addr}/api.json")
    })
    .clone()
}

/// A mock model on its own runtime: the test thread blocks in `radian arc work`, so the
/// server must serve from worker threads while the test waits.
pub struct Mock {
    pub server: MockServer,
    runtime: tokio::runtime::Runtime,
}

impl Mock {
    pub fn start(replies: Vec<Reply>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the mock runtime");
        let server = runtime.block_on(MockServer::start(replies));
        Self { server, runtime }
    }

    pub fn base_url(&self) -> &str {
        &self.server.base_url
    }

    pub fn requests(&self) -> Vec<agent_provider_openai_compatible::testing::RecordedRequest> {
        self.server.requests()
    }

    /// Every request body as one string, for a "the secret never reached the model" check.
    pub fn request_bodies(&self) -> String {
        self.requests()
            .iter()
            .map(|request| request.json().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One temp repository: a bare remote and a main clone with `<remote>/<main>` checked out.
pub struct Repo {
    pub root: PathBuf,
    pub home: PathBuf,
    pub sessions: PathBuf,
    pub origin: PathBuf,
    pub clone: PathBuf,
    /// The remote the wrapper fetches and pushes (`--remote`).
    pub remote: String,
    /// The main branch the wrapper starts from (`--main-branch`).
    pub main_branch: String,
    /// Owns `root` and removes the whole scratch tree on drop, including on a panic.
    _dir: TempDir,
}

impl Repo {
    pub fn new(prefix: &str) -> Self {
        Self::with(prefix, "origin", "main")
    }

    /// A repository whose remote and main branch are not the defaults, so a hardcoded
    /// `origin`/`main` fails to find anything.
    pub fn with(prefix: &str, remote: &str, main_branch: &str) -> Self {
        let dir = scratch(prefix);
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&home).unwrap();
        // Every process these tests spawn inherits this as `HOME` and `RADIAN_HOME`, so the
        // control port is off for all of them before anything runs; see
        // [`SCRATCH_HOME_CONFIG`]. Tests that write a `config.toml` later write the arc
        // store's own (`<clone>/.radian/arc/config.toml`), not this one.
        let config_dir = home.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), SCRATCH_HOME_CONFIG).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let origin = root.join("origin.git");
        let clone = root.join("clone");
        std::fs::create_dir_all(&clone).unwrap();
        let repo = Self {
            root,
            home,
            sessions,
            origin,
            clone,
            remote: remote.to_owned(),
            main_branch: main_branch.to_owned(),
            _dir: dir,
        };
        repo.git_in(
            &repo.root,
            &[
                "init",
                "--bare",
                "-b",
                main_branch,
                repo.origin.to_str().unwrap(),
            ],
        );
        repo.git(&["init", "-b", main_branch]);
        std::fs::write(repo.clone.join("README.md"), "initial\n").unwrap();
        repo.git(&["config", "user.email", "arc@example.invalid"]);
        repo.git(&["config", "user.name", "arc test"]);
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-m", "initial"]);
        repo.git(&["remote", "add", remote, repo.origin.to_str().unwrap()]);
        repo.git(&["push", "-u", remote, main_branch]);
        repo.git(&["fetch", remote]);
        repo
    }

    /// Adds a remote that points at a path with no repository, so a wrapper that used this
    /// name instead of the configured remote would fail at the fetch or the push.
    pub fn dead_remote(&self, name: &str) {
        let path = self.root.join("nowhere.git");
        self.git(&["remote", "add", name, path.to_str().unwrap()]);
    }

    pub fn arc(&self) -> PathBuf {
        self.clone.join(".radian").join("arc")
    }

    /// The clone as arc records it. arc canonicalizes the repository root
    /// (`store::git::repository_root`), which on macOS turns a `$TMPDIR` under `/var` into
    /// `/private/var`; a test that compares a recorded path must start from this.
    pub fn canonical_clone(&self) -> PathBuf {
        fs::canonicalize(&self.clone).expect("the clone canonicalizes")
    }

    pub fn worktree(&self, task: &str) -> PathBuf {
        self.arc().join("worktrees").join(task)
    }

    pub fn attempt_dir(&self, task: &str, attempt: u32) -> PathBuf {
        self.arc()
            .join("attempts")
            .join(task)
            .join(attempt.to_string())
    }

    pub fn run_dir(&self, task: &str, attempt: u32) -> PathBuf {
        self.arc()
            .join("runs")
            .join(task)
            .join("attempts")
            .join(format!("{task}-{attempt}"))
    }

    /// Runs one git command in the clone with the test's isolated environment.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.clone, args)
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} in {} failed: {} (exit code {:?})\nstdout:\n{}\nstderr:\n{}",
            dir.display(),
            output.status,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The sha of the local `work/<task>` branch, if it exists.
    pub fn local_branch(&self, task: &str) -> Option<String> {
        let output = Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/work/{task}"),
            ])
            .current_dir(&self.clone)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .output()
            .expect("run git");
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// The sha `origin` has for `work/<task>`, if any.
    pub fn remote_branch(&self, task: &str) -> Option<String> {
        let output = Command::new("git")
            .args([
                "ls-remote",
                &self.remote,
                &format!("refs/heads/work/{task}"),
            ])
            .current_dir(&self.clone)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .output()
            .expect("run git");
        assert!(output.status.success(), "ls-remote failed");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .map(str::to_owned)
    }

    /// The base commit the wrapper starts from (`<remote>/<main>` at fixture creation).
    pub fn main_sha(&self) -> String {
        self.git(&[
            "rev-parse",
            &format!("{}/{}", self.remote, self.main_branch),
        ])
        .trim()
        .to_owned()
    }

    /// The remote's current main tip, read over the wire (not the local tracking ref).
    pub fn remote_main_sha(&self) -> String {
        let output = Command::new("git")
            .args([
                "ls-remote",
                &self.remote,
                &format!("refs/heads/{}", self.main_branch),
            ])
            .current_dir(&self.clone)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .output()
            .expect("run git");
        assert!(output.status.success(), "ls-remote failed");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    /// Points `core.hooksPath` at a directory with `pre-push` and `post-checkout` hooks that
    /// each write a marker. The wrapper's own git commands must run none of them.
    pub fn install_hooks(&self) -> (PathBuf, PathBuf) {
        let hooks = self.root.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let pre_push = self.root.join("marker-pre-push");
        let post_checkout = self.root.join("marker-post-checkout");
        write_hook(&hooks.join("pre-push"), &pre_push);
        write_hook(&hooks.join("post-checkout"), &post_checkout);
        self.git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
        (pre_push, post_checkout)
    }

    /// A second clone of the bare remote, for a commit the main clone has not fetched yet.
    pub fn second_clone(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        let output = Command::new("git")
            .args([
                "clone",
                self.origin.to_str().unwrap(),
                dir.to_str().unwrap(),
            ])
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .output()
            .expect("run git clone");
        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        dir
    }

    /// Runs one git command in `dir` with the test's isolated environment.
    pub fn git_at(&self, dir: &Path, args: &[&str]) -> String {
        self.git_in(dir, args)
    }

    /// Runs `radian arc work` from the clone with an isolated environment. A repository whose
    /// remote or main branch is not the default passes its values as the wrapper's internal
    /// flags; the defaults themselves come from the repository's `config.toml` when
    /// `radian arc init` has run, else `origin`/`main`.
    pub fn work(&self, args: &[&str], brief: &str) -> Run {
        let mut full = vec!["arc", "work"];
        if self.remote != "origin" {
            full.push("--remote");
            full.push(&self.remote);
        }
        if self.main_branch != "main" {
            full.push("--main-branch");
            full.push(&self.main_branch);
        }
        full.extend_from_slice(args);
        let mut child = self
            .radian_command(&full)
            .spawn()
            .expect("spawn radian arc work");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(brief.as_bytes())
            .expect("write the brief");
        let output = child.wait_with_output().expect("wait for radian arc work");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Runs one `radian` command in the clone with the test's isolated environment and no
    /// stdin (the `radian arc init`, `list`, ... subcommands).
    pub fn radian(&self, args: &[&str]) -> Run {
        let output = self
            .radian_command(args)
            .stdin(Stdio::null())
            .output()
            .expect("run radian");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Like [`Repo::radian`], with extra environment variables that win over the isolated
    /// defaults. A test that must change `PATH` (or another process-wide variable) sets it
    /// here, on the child, instead of calling `std::env::set_var` in the test process.
    pub fn radian_with_env(&self, args: &[&str], envs: &[(&str, String)]) -> Run {
        let mut command = self.radian_command(args);
        for (key, value) in envs {
            command.env(key, value);
        }
        let output = command.stdin(Stdio::null()).output().expect("run radian");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Runs one `radian` command in the clone and returns the running child: a test that needs
    /// to signal the process, or to run something else while it blocks, uses this.
    pub fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.radian_command(args).spawn().expect("spawn radian")
    }

    /// Like [`Repo::spawn`], in a process group of its own: a test can signal that group the
    /// way a terminal's Ctrl-C signals the foreground job.
    pub fn spawn_in_own_group(&self, args: &[&str]) -> std::process::Child {
        self.spawn_in_own_group_with_env(args, &[])
    }

    /// [`Repo::spawn_in_own_group`] with extra environment variables that win over the isolated
    /// defaults.
    pub fn spawn_in_own_group_with_env(
        &self,
        args: &[&str],
        envs: &[(&str, String)],
    ) -> std::process::Child {
        let mut command = self.radian_command(args);
        for (key, value) in envs {
            command.env(key, value);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        command.spawn().expect("spawn radian")
    }

    /// The `radian` binary in the clone, with the environment isolated from the machine: the
    /// home, the session directory and the model catalog are the test's own.
    fn radian_command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(radian_bin());
        command
            .args(args)
            .current_dir(&self.clone)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("RADIAN_HOME", &self.home)
            .env("RADIAN_SESSION_DIR", &self.sessions)
            .env("RADIAN_CATALOG_URL", fast_catalog_url())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

/// A fake `radian exec` for the review stage. It records its argv, prompt and pid per run and
/// prints one `Result` line whose `text` is the run's chosen message. Every run touches
/// `<dir>/started-<n>` when it starts, so a test can see a session running.
pub struct FakeReview {
    dir: TempDir,
    program: PathBuf,
}

impl FakeReview {
    /// `messages[n]` is run `n`'s final text; a missing entry repeats the last message.
    pub fn new(label: &str, messages: &[String], sleep_secs: u32) -> Self {
        Self::build(label, messages, sleep_secs, None, false)
    }

    /// Like [`FakeReview::new`], but the `Result` line echoes the `--session-id` the stage
    /// passed (falling back to `fake-review-<n>` when it passed none), so a test can assert the
    /// verdict's `review_session_ids` names the session the stage asked for.
    pub fn with_session_echo(label: &str, messages: &[String], sleep_secs: u32) -> Self {
        Self::build(label, messages, sleep_secs, None, true)
    }

    /// [`FakeReview::new`] with `barrier` = `(wait_for, seconds)`: the run waits up to `seconds`
    /// for that file to appear before it answers. The `commands` stage runs its own script, so
    /// two programs can wait for each other and prove they started together.
    pub fn with_barrier(
        label: &str,
        messages: &[String],
        sleep_secs: u32,
        barrier: Option<(&Path, u32)>,
    ) -> Self {
        Self::build(label, messages, sleep_secs, barrier, false)
    }

    fn build(
        label: &str,
        messages: &[String],
        sleep_secs: u32,
        barrier: Option<(&Path, u32)>,
        echo_session_id: bool,
    ) -> Self {
        let dir = TempDir::new(label);
        for (index, message) in messages.iter().enumerate() {
            fs::write(
                dir.path().join(format!("text-{}", index + 1)),
                serde_json::to_string(message).expect("a JSON string"),
            )
            .expect("the fake message");
        }
        let last = messages.len().max(1);
        for run in 1..=last {
            fs::write(
                dir.path().join(format!("sleep-{run}")),
                sleep_secs.to_string(),
            )
            .expect("the fake sleep");
        }
        let wait = match barrier {
            Some((path, seconds)) => format!(
                "i=0\n\
                 while [ ! -f '{path}' ] && [ $i -lt {loops} ]; do i=$((i+1)); sleep 0.05; done\n",
                path = path.display(),
                loops = seconds.saturating_mul(20),
            ),
            None => String::new(),
        };
        let session = if echo_session_id {
            "sid=\"\"\n\
             previous=\"\"\n\
             for argument in \"$@\"; do\n\
               if [ \"$previous\" = \"--session-id\" ]; then sid=\"$argument\"; fi\n\
               previous=\"$argument\"\n\
             done\n\
             if [ -z \"$sid\" ]; then sid=\"fake-review-$n\"; fi\n"
        } else {
            "sid=\"fake-review-$n\"\n"
        };
        let program = dir.path().join("fake-radian-exec");
        let script = format!(
            "#!/bin/sh\n\
             d=\"{dir}\"\n\
             n=$(cat \"$d/count\" 2>/dev/null || printf '0')\n\
             n=$((n+1))\n\
             printf '%s\\n' \"$n\" > \"$d/count\"\n\
             printf '%s\\n' \"$@\" > \"$d/argv-$n\"\n\
             printf '%s\\n' \"$$\" > \"$d/pid-$n\"\n\
             touch \"$d/started-$n\"\n\
             cat > \"$d/prompt-$n\"\n\
             {session}\
             {wait}\
             s=$(cat \"$d/sleep-$n\" 2>/dev/null || printf '{sleep_secs}')\n\
             sleep \"$s\"\n\
             if [ ! -f \"$d/text-$n\" ]; then n={last}; fi\n\
             me=$(cat \"$d/model-error-$n.json\" 2>/dev/null || true)\n\
             if [ -n \"$me\" ]; then outcome=failed; else outcome=completed; fi\n\
             printf '{{\"type\":\"Result\",\"session_id\":\"%s\",\"outcome\":\"%s\",\"cost\":{{\"micros\":2000}},\"text\":' \"$sid\" \"$outcome\"\n\
             cat \"$d/text-$n\"\n\
             if [ -n \"$me\" ]; then printf ',\"model_error\":%s' \"$me\"; fi\n\
             printf '}}\\n'\n\
             if [ -n \"$me\" ]; then exit 1; fi\n",
            dir = dir.path().display(),
            session = session,
            wait = wait,
            sleep_secs = sleep_secs,
            last = last,
        );
        fs::write(&program, script).expect("the fake program");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&program)
                .expect("the fake program")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&program, permissions).expect("make it executable");
        }
        Self { dir, program }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn count(&self) -> u32 {
        fs::read_to_string(self.dir.path().join("count"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn argv(&self, run: u32) -> Vec<String> {
        fs::read_to_string(self.dir.path().join(format!("argv-{run}")))
            .expect("the fake recorded its argv")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn prompt(&self, run: u32) -> String {
        fs::read_to_string(self.dir.path().join(format!("prompt-{run}")))
            .expect("the fake recorded its prompt")
    }

    /// The marker a run touches when it starts.
    pub fn started(&self, run: u32) -> PathBuf {
        self.dir.path().join(format!("started-{run}"))
    }

    /// The pid of a run's shell, which leads its own process group.
    pub fn pid(&self, run: u32) -> u32 {
        fs::read_to_string(self.dir.path().join(format!("pid-{run}")))
            .expect("the fake recorded its pid")
            .trim()
            .parse()
            .expect("a pid")
    }

    /// Makes every run report this `Result.cost.micros` instead of the default 2000. The
    /// program is a one-test script, so the test that changes it owns it.
    pub fn set_cost(&self, micros: u64) {
        let text = fs::read_to_string(&self.program).expect("the fake program");
        assert!(
            text.contains("\"micros\":2000"),
            "the fake's default cost is in its program"
        );
        fs::write(
            &self.program,
            text.replacen("\"micros\":2000", &format!("\"micros\":{micros}"), 1),
        )
        .expect("the fake cost");
    }

    /// Makes every run report `"cost": null`, as a session with no cost does.
    pub fn set_no_cost(&self) {
        let text = fs::read_to_string(&self.program).expect("the fake program");
        let replaced = text.replacen("\"cost\":{\"micros\":2000}", "\"cost\":null", 1);
        assert_ne!(replaced, text, "the fake's default cost is in its program");
        fs::write(&self.program, replaced).expect("the fake cost");
    }

    /// Makes `run` print a failed `Result` carrying `model_error` (ADR 0044 §2b): the outcome
    /// is `failed`, the process exits `1`, and the object is copied as given. The run's text
    /// is still its chosen message; the stage reads the error rather than the text.
    pub fn set_model_error(&self, run: u32, error: &serde_json::Value) {
        fs::write(
            self.dir.path().join(format!("model-error-{run}.json")),
            serde_json::to_string(error).expect("the model error serializes"),
        )
        .expect("the fake model error");
    }

    /// Changes how long a later run sleeps before it answers.
    pub fn set_sleep(&self, run: u32, seconds: u32) {
        fs::write(
            self.dir.path().join(format!("sleep-{run}")),
            seconds.to_string(),
        )
        .expect("the fake sleep");
    }
}

/// A remote that speaks the git protocol through an `ext::` script: the service named by
/// `hang` (`upload` for the fetch, `receive` for the push, or `both`) records its own pid and
/// its child's, then sleeps; the other service serves the bare `origin` normally. A test uses
/// it to make one remote call wait past `[limits] git_remote_timeout_secs` while the others
/// still work (ADR 0048 decision 7.8).
pub struct HangRemote {
    dir: PathBuf,
}

impl HangRemote {
    /// Rewires `repo`'s `origin` to the hanging remote. `hang` is `upload`, `receive` or
    /// `both`.
    pub fn install(repo: &TestRepo, label: &str, hang: &str) -> Self {
        let dir = repo.repo.with_file_name(format!("hang-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ext.sh");
        let origin = path(&repo.origin);
        let text = format!(
            "#!/bin/sh\n\
             ROOT=\"{root}\"\n\
             hang() {{\n\
               echo $$ > \"$ROOT/server.pid\"\n\
               sleep 300 &\n\
               echo $! > \"$ROOT/child.pid\"\n\
               wait\n\
             }}\n\
             case \"$GIT_EXT_SERVICE\" in\n\
               git-upload-pack)\n\
                 if [ \"{hang}\" = both ] || [ \"{hang}\" = upload ]; then hang; \
                 else exec git upload-pack \"{origin}\"; fi ;;\n\
               git-receive-pack)\n\
                 if [ \"{hang}\" = both ] || [ \"{hang}\" = receive ]; then hang; \
                 else exec git receive-pack \"{origin}\"; fi ;;\n\
               *) exit 1 ;;\n\
             esac\n",
            root = dir.display(),
            origin = origin,
            hang = hang,
        );
        std::fs::write(&script, text).expect("the hanging remote script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script)
                .expect("the script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).expect("make it executable");
        }
        repo.git(&["config", "protocol.ext.allow", "always"]);
        repo.git(&[
            "remote",
            "set-url",
            "origin",
            &format!("ext::{}", script.display()),
        ]);
        Self { dir }
    }

    /// The pid of the child the hanging service spawned, once it has written it.
    pub fn child_pid(&self) -> u32 {
        self.pid("child.pid")
    }

    /// The pid of the hanging service itself.
    pub fn server_pid(&self) -> u32 {
        self.pid("server.pid")
    }

    fn pid(&self, name: &str) -> u32 {
        let path = self.dir.join(name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the hanging remote never wrote {}: it did not run",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Asserts that `pid` is gone, so the timeout killed the whole process group and not only
    /// the git process.
    pub fn assert_gone(&self, pid: u32, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if !alive(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("{what} ({pid}) is still alive; the timeout did not kill its process group");
    }
}

/// Whether `pid` exists: `kill(pid, 0)`, never the `kill` program.
pub fn alive(pid: u32) -> bool {
    // SAFETY: `kill(2)` with a positive pid and signal 0 reads no memory.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A session's final message with one fenced report.
pub fn review_message(summary: &str, blocking: &[&str], suggestions: &[&str]) -> String {
    let value = serde_json::json!({
        "summary": summary,
        "blocking": blocking,
        "suggestions": suggestions,
        "verified": ["the diff reads correctly"],
    });
    format!(
        "Here is my review.\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(&value).expect("the report serializes")
    )
}

fn write_hook(path: &Path, marker: &Path) {
    std::fs::write(path, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// The wrapper's `AttemptFinished` record: its last stdout line.
    pub fn finished(&self) -> serde_json::Value {
        let line = self
            .stdout
            .lines()
            .last()
            .unwrap_or_else(|| panic!("no stdout: {}", self.stderr));
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}"));
        assert_eq!(
            value.get("type"),
            Some(&serde_json::Value::String("AttemptFinished".to_owned())),
            "the last stdout line is AttemptFinished: {line}"
        );
        value
    }
}

pub fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        agent_provider_openai_compatible::testing::chunk(
            serde_json::json!({"role": "assistant", "content": text}),
            None,
        ),
        agent_provider_openai_compatible::testing::chunk(serde_json::json!({}), Some("stop")),
    ])
}

pub fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        agent_provider_openai_compatible::testing::chunk(
            serde_json::json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        agent_provider_openai_compatible::testing::chunk(serde_json::json!({}), Some("tool_calls")),
    ])
}

/// A `shell` tool call that runs `command` in the worker's workspace.
pub fn shell_reply(id: &str, command: &str) -> Reply {
    tool_reply(
        id,
        "shell",
        &serde_json::json!({"command": command}).to_string(),
    )
}

/// A `write` tool call.
pub fn write_reply(id: &str, path: &str, content: &str) -> Reply {
    tool_reply(
        id,
        "write",
        &serde_json::json!({"path": path, "content": content}).to_string(),
    )
}

/// A `read` tool call.
pub fn read_reply(id: &str, path: &str) -> Reply {
    tool_reply(id, "read", &serde_json::json!({"path": path}).to_string())
}

/// The model route every test passes after `--`.
pub fn model_args(base_url: &str) -> Vec<String> {
    vec![
        "--model".to_owned(),
        "m".to_owned(),
        "--base-url".to_owned(),
        base_url.to_owned(),
    ]
}

/// `work`'s argument list for one attempt: the required flags, then any boolean flags
/// (`--resume`, `--allow-noop`), then `--` and the extra `radian acp` arguments.
pub fn work_args(
    task: &str,
    attempt: u32,
    token: &str,
    flags: &[&str],
    extra: &[String],
) -> Vec<String> {
    let mut args = vec![
        "--task".to_owned(),
        task.to_owned(),
        "--attempt".to_owned(),
        attempt.to_string(),
        "--token".to_owned(),
        token.to_owned(),
    ];
    args.extend(flags.iter().map(|flag| (*flag).to_owned()));
    args.push("--".to_owned());
    args.extend(extra.iter().cloned());
    args
}

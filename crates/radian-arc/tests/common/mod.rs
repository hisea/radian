//! Shared helpers for the `radian-arc` integration tests.
//!
//! [`TestRepo`] is the small repository with a bare `origin` the gate tests use; [`Repo`] is
//! the bare remote, main clone and isolated home the `radian arc work` tests use. Every work
//! test runs a real `radian` binary and a real `radian exec` child, driven by the mock model
//! in `agent-provider-openai-compatible`'s `testing` feature.
#![allow(dead_code)]

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
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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
}

pub fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

pub fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
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
    let output = command.output().expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `/tmp` (not `$TMPDIR`): on macOS the latter can push a control socket path past `SUN_LEN`.
pub fn scratch(prefix: &str) -> PathBuf {
    static DIRS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = DIRS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = PathBuf::from("/tmp").join(format!(
        "radian-arc-work-{prefix}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
}

impl Repo {
    pub fn new(prefix: &str) -> Self {
        Self::with(prefix, "origin", "main")
    }

    /// A repository whose remote and main branch are not the defaults, so a hardcoded
    /// `origin`/`main` fails to find anything.
    pub fn with(prefix: &str, remote: &str, main_branch: &str) -> Self {
        let root = scratch(prefix);
        let home = root.join("home");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&home).unwrap();
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
            .join(attempt.to_string())
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
            "git {args:?} failed: {}",
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

    /// Runs one `radian` command in the clone and returns the running child: a test that needs
    /// to signal the process, or to run something else while it blocks, uses this.
    pub fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.radian_command(args).spawn().expect("spawn radian")
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
/// (`--resume`, `--allow-noop`), then `--` and the extra `radian exec` arguments.
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

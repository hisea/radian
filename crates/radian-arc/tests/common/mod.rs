#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

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

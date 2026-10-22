//! ADR 0038 and ADR 0048 decision 5 on the arc worker layout: the workspace is a linked
//! worktree **inside** its repository (`<repository>/.radian/arc/worktrees/<task>`), so the
//! shared common git dir is `<repository>/.git`, and every other entry of
//! `<repository>/.radian/arc/` is on the sandbox deny-read list.
//!
//! Under [`GitAccess::WritableExceptExec`] the worker commits with native git (refs, index and
//! objects of the common dir are writable) while the entries git executes stay read-only and
//! the denied arc paths stay unreadable.
//!
//! This is the Linux regression: before the bwrap carve-out the whole `.git` was one
//! `--ro-bind`, so `git add` failed with `fatal: Unable to create
//! '…/.git/worktrees/<task>/index.lock': Read-only file system`. On macOS the same test passed
//! before the fix, which is why it is not gated to Linux.
//!
//! Every command runs under `FullAccess` in an identical second tree as the control, so a
//! failure is never mistaken for a missing file or a typo.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_sandbox::{GitAccess, Sandbox, SandboxMode, SandboxPolicy};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    /// A repository with one commit and a linked worktree at
    /// `<repository>/.radian/arc/worktrees/0001-task`, plus the arc entries the wrapper
    /// denies (a `plans/` directory and `config.toml`).
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "radian-arc-sandbox-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &["config", "user.email", "arc-sandbox@example.invalid"],
        );
        git(&repo, &["config", "user.name", "arc sandbox"]);
        // A quiet, deterministic repository: no background auto-maintenance, no hooks.
        git(&repo, &["config", "maintenance.auto", "false"]);
        git(&repo, &["config", "gc.auto", "0"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-qm", "base"]);

        let worktree = repo
            .join(".radian")
            .join("arc")
            .join("worktrees")
            .join("0001-task");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                "work/0001-task",
            ],
        );

        // The arc entries the wrapper denies: everything under `.radian/arc/` except
        // `worktrees/`.
        let plans = repo.join(".radian").join("arc").join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("secret.md"), "ARC-PLAN-SECRET\n").unwrap();
        std::fs::write(
            repo.join(".radian").join("arc").join("config.toml"),
            "ARC-CONFIG-SECRET\n",
        )
        .unwrap();

        Self { root }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn worktree(&self) -> PathBuf {
        self.repo().join(".radian/arc/worktrees/0001-task")
    }

    fn plans(&self) -> PathBuf {
        self.repo().join(".radian/arc/plans")
    }

    fn config(&self) -> PathBuf {
        self.repo().join(".radian/arc/config.toml")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git is required for this test");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Runs `script` under `sandbox` and returns the output.
fn run(sandbox: &Sandbox, script: &str) -> Output {
    sandbox
        .command("/bin/sh", ["-c", script])
        .expect("the sandbox builds a command")
        .output()
        .expect("the sandbox runs the command")
}

fn text(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

fn sandbox_for(worktree: &Path, deny: &[PathBuf]) -> Sandbox {
    let mut policy = SandboxPolicy::workspace_write(worktree)
        .expect("the worktree is a directory")
        .git_access(GitAccess::WritableExceptExec);
    for path in deny {
        policy = policy.deny_read(path);
    }
    Sandbox::from_policy(policy).unwrap_or_else(|error| {
        panic!("the `workspace` sandbox backend must work for this test: {error}")
    })
}

#[test]
fn a_worker_commits_in_a_linked_worktree_nested_inside_its_repository() {
    let sandbox_scratch = Scratch::new("sandbox");
    let control_scratch = Scratch::new("control");
    let delay = vec![sandbox_scratch.plans(), sandbox_scratch.config()];
    let sandbox = sandbox_for(&sandbox_scratch.worktree(), &delay);
    let control = Sandbox::new(SandboxMode::FullAccess, control_scratch.worktree())
        .expect("the control has no sandbox");

    assert!(
        sandbox.git_writable_except_exec(),
        "the backend must report the carve-out it enforces"
    );

    // The worker's own command: a write inside the worktree, `git add`, `git commit`. The
    // commit writes the per-worktree index and the shared common dir's refs and object store.
    let commit = "printf 'change\\n' > change.txt && git add change.txt && git commit -qm change";
    let output = run(&sandbox, commit);
    assert!(
        output.status.success(),
        "the sandboxed worker cannot commit: {}",
        text(&output)
    );
    assert!(
        run(&control, commit).status.success(),
        "the control cannot commit; the script is not sound"
    );

    // The denied arc paths stay unreadable. A directory deny fails outright on both backends;
    // a file deny is a mask on Linux (`/dev/null`), so the marker is what must not appear.
    let secret = sandbox_scratch.plans().join("secret.md");
    let output = run(&sandbox, &format!("cat {}", q(&secret)));
    assert!(
        !output.status.success() && !text(&output).contains("ARC-PLAN-SECRET"),
        "the sandbox read the denied plan: {}",
        text(&output)
    );
    let output = run(&control, &format!("cat {}", q(&secret)));
    assert!(
        output.status.success() && text(&output).contains("ARC-PLAN-SECRET"),
        "the control cannot read the plan; the denial proves nothing: {}",
        text(&output)
    );
    let output = run(&sandbox, &format!("cat {}", q(&sandbox_scratch.config())));
    assert!(
        !text(&output).contains("ARC-CONFIG-SECRET"),
        "the sandbox read the denied arc config: {}",
        text(&output)
    );

    // The entries git can execute stay read-only: `config`, `hooks/` and the worktree's
    // `.git` pointer file (writing it would repoint git at a gitdir the model controls).
    let repo = sandbox_scratch.repo();
    let common = repo.join(".git");
    let control_repo = control_scratch.repo();
    let control_common = control_repo.join(".git");
    let denied = [
        (
            format!(
                "cd {} && git config core.sbx-harness 1",
                q(&sandbox_scratch.worktree())
            ),
            format!(
                "cd {} && git config core.sbx-harness 1",
                q(&control_scratch.worktree())
            ),
            "config",
        ),
        (
            format!("printf x > {}", q(&common.join("hooks/sbx-new-hook"))),
            format!(
                "printf x > {}",
                q(&control_common.join("hooks/sbx-new-hook"))
            ),
            "hooks/",
        ),
    ];
    for (sandboxed, controlled, name) in denied {
        assert!(
            !run(&sandbox, &sandboxed).status.success(),
            "the sandbox wrote {name}"
        );
        assert!(
            run(&control, &controlled).status.success(),
            "the control could not write {name}; the denial proves nothing"
        );
    }

    // Everything else under the shared `.git` (the object store, refs, `info/`) stays writable;
    // without it the commit above could not have worked.
    assert!(
        run(
            &sandbox,
            &format!("printf x > {}", q(&common.join("objects/sbx-object")))
        )
        .status
        .success(),
        "the object store must stay writable"
    );

    // The worktree's `.git` is a pointer file; last, because writing it in the control breaks
    // that tree.
    assert!(
        !run(
            &sandbox,
            &format!("printf x > {}", q(&sandbox_scratch.worktree().join(".git")))
        )
        .status
        .success(),
        "the sandbox repointed the worktree's .git"
    );
    assert!(
        run(
            &control,
            &format!("printf x > {}", q(&control_scratch.worktree().join(".git")))
        )
        .status
        .success(),
        "the control could not repoint the pointer file; the denial proves nothing"
    );
}

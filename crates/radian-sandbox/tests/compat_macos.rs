//! macOS compatibility round: what do ordinary commands need under `workspace-write` with
//! the default sensitive-read denies? Findings feed the allowlist; anything incompatible is
//! recorded, never fixed by widening a rule (ADR 0012 decision 3).
//!
//! ADR 0038 decision 4: the `workspace` tier a general session runs in asks for
//! `WritableExceptExec`, so this round now builds that policy — `git commit` and `git add`
//! work, while writing `.git/config` stays refused. The read-only rule a task agent keeps is
//! covered by the escape matrix's `git_readonly` group and `tests/git_writable.rs`.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use radian_sandbox::{GitAccess, Sandbox, SandboxPolicy};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("radian-compat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn outcome(sandbox: &Sandbox, program: &str, args: &[&str]) -> (bool, String) {
    let mut command = sandbox.command(program, args).expect("sandbox command");
    match command.output() {
        Ok(output) => {
            let mut detail = String::new();
            for stream in [&output.stdout, &output.stderr] {
                let text = String::from_utf8_lossy(stream);
                let text = text.trim();
                if !text.is_empty() {
                    if !detail.is_empty() {
                        detail.push_str(" | ");
                    }
                    detail.push_str(text);
                }
            }
            (output.status.success(), detail)
        }
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

#[test]
fn workspace_write_is_compatible_with_the_common_commands() {
    let scratch = Scratch::new();
    let repo = scratch.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "compat@example.com"]);
    git(&repo, &["config", "user.name", "compat"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "init"]);

    // ADR 0038 decision 4: the `workspace` tier a general session runs in asks for
    // `WritableExceptExec`, so `.git` is writable except the entries git executes. The compat
    // round exercises that policy, not the read-only default a task agent keeps.
    let policy = SandboxPolicy::workspace_write(&repo)
        .unwrap()
        .git_access(GitAccess::WritableExceptExec);
    let sandbox = Sandbox::from_policy(policy).unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    let mut findings = Vec::new();
    for (name, program, args, must_succeed) in [
        (
            "git status",
            "git",
            vec!["--no-optional-locks", "status", "--porcelain"],
            true,
        ),
        ("git log", "git", vec!["log", "--oneline", "-1"], true),
        (
            "git config get",
            "git",
            vec!["config", "--get", "user.email"],
            true,
        ),
        // ADR 0038: a general session may commit in the workspace tier.
        (
            "git commit",
            "git",
            vec!["commit", "--allow-empty", "-qm", "x"],
            true,
        ),
        ("git add", "git", vec!["add", "a.txt"], true),
        // The escape hatch stays shut: writing `.git/config` (here `core.hooksPath`, which
        // would run the model's own hooks) is refused by the exec-entry carve-out.
        (
            "git config set",
            "git",
            vec!["config", "core.hooksPath", "/tmp/elsewhere"],
            false,
        ),
        ("ssh -V", "ssh", vec!["-V"], true),
        ("cargo --version", &cargo, vec!["--version"], true),
    ] {
        let (ok, detail) = outcome(&sandbox, program, &args);
        findings.push(format!(
            "{name:<16} {}  {}",
            if ok { "ok" } else { "FAILED" },
            detail.chars().take(120).collect::<String>()
        ));
        if must_succeed {
            assert!(ok, "{name} should work under workspace-write: {detail}");
        } else {
            // Expected to be refused by the `.git` exec-entry carve-out or the write boundary.
            assert!(
                !ok,
                "{name} unexpectedly succeeded; the write boundary or .git carve-out is not binding"
            );
        }
    }

    // `cargo build` in a dependency-free project. It writes `target/` inside the workspace,
    // reads `~/.cargo`/`~/.rustup`, and uses TMPDIR (kept inside the workspace).
    let project = scratch.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"compat-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir(project.join("src")).unwrap();
    std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
    let policy = SandboxPolicy::workspace_write(&project).unwrap();
    let sandbox = Sandbox::from_policy(policy).unwrap();
    let (build_ok, build_detail) = outcome(&sandbox, &cargo, &["build", "--offline", "-q"]);
    findings.push(format!(
        "{:<16} {}  {}",
        "cargo build",
        if build_ok { "ok" } else { "FAILED" },
        build_detail.chars().take(400).collect::<String>()
    ));

    eprintln!("\ncompatibility findings (macOS):\n{}", findings.join("\n"));
    assert!(
        build_ok,
        "cargo build must work under workspace-write; it said: {build_detail}"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

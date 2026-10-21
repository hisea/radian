//! Linux compatibility round, mirroring `compat_macos.rs`: what do ordinary commands need
//! under `workspace-write` with the default sensitive-read denies? Anything incompatible is
//! recorded, never fixed by widening a rule (ADR 0012 P4).
//!
//! Whether a **dependency-bearing** `cargo build` can run in
//! the Linux sandbox (macOS was verified, Linux was not). The last case answers that with a
//! real project that depends on `itoa` and builds `--offline` from the cargo cache the test
//! run already populated.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

use radian_sandbox::{Sandbox, SandboxPolicy};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("radian-compat-linux-{}", std::process::id()));
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

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The cargo home the test process uses (rustup's shim needs its home to find the
/// toolchain). On CI these are `~/.cargo`/`~/.rustup` and already readable; on the review VM
/// they live under `/tmp`, so they are named explicitly.
fn with_cargo_homes(policy: SandboxPolicy) -> SandboxPolicy {
    ["CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .fold(policy, |policy, name| policy.allow_env(name))
}

#[test]
fn workspace_write_is_compatible_with_the_common_commands_and_a_real_cargo_build() {
    let scratch = Scratch::new();
    let repo = scratch.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "compat@example.com"]);
    git(&repo, &["config", "user.name", "compat"]);
    // Pin against git's background auto-maintenance (see tests/git.rs): it must not race the
    // `.git` assertions.
    git(&repo, &["config", "maintenance.auto", "false"]);
    git(&repo, &["config", "gc.auto", "0"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-qm", "init"]);

    let policy = SandboxPolicy::workspace_write(&repo).unwrap();
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
        (
            "git commit",
            "git",
            vec!["commit", "--allow-empty", "-qm", "x"],
            false,
        ),
        ("git add", "git", vec!["add", "a.txt"], false),
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
            // Expected to be refused by the read-only `.git` carve-out or write boundary.
            assert!(
                !ok,
                "{name} unexpectedly succeeded; the write boundary or .git carve-out is not binding"
            );
        }
    }
    eprintln!("{}", findings.join("\n"));

    // A real, dependency-bearing build. `cargo build --offline` writes `target/` inside the
    // workspace, reads the cargo/rustup homes (read-only under the root), and uses the
    // workspace TMPDIR.
    let project = scratch.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"compat-linux-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nitoa = \"1\"\n",
    )
    .unwrap();
    std::fs::create_dir(project.join("src")).unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() { let mut b = itoa::Buffer::new(); println!(\"{}\", b.format(12345)); }\n",
    )
    .unwrap();
    // Resolve the lockfile outside the sandbox so the build itself needs no index writes.
    let lock = Command::new(&cargo)
        .arg("generate-lockfile")
        .arg("--offline")
        .current_dir(&project)
        .output()
        .expect("cargo must run");
    assert!(
        lock.status.success(),
        "cargo generate-lockfile --offline failed: {}",
        String::from_utf8_lossy(&lock.stderr)
    );

    let policy = with_cargo_homes(SandboxPolicy::workspace_write(&project).unwrap());
    let sandbox = Sandbox::from_policy(policy).unwrap();
    let (ok, detail) = outcome(&sandbox, &cargo, &["build", "--offline"]);
    assert!(
        ok,
        "a dependency-bearing `cargo build --offline` must work under workspace-write: {detail}"
    );
    assert!(
        project.join("target/debug/compat-linux-probe").exists(),
        "the binary must be built inside the workspace"
    );
}

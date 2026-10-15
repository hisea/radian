//! ADR 0038 slice 1 (macOS): under [`GitAccess::WritableExceptExec`] a general session may
//! write `.git` except the entries git can execute, so native git runs in the sandbox.
//!
//! Real `sandbox-exec`; every command is also run under `FullAccess` as a control, so a
//! sandbox failure is never mistaken for a missing file or a typo (the escape matrix's rule).
//! This is macOS-only on purpose: ADR 0038 slice 1 leaves the Linux backend on today's
//! behaviour (the whole `.git` read-only) until the VM matrix can run.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_sandbox::{GitAccess, Sandbox, SandboxMode, SandboxPolicy};

/// A scratch tree: a workspace with a repository that has a submodule, and a worktree whose
/// shared common dir lives **inside** the workspace, so the ADR 0038 carve-out is exercised by
/// the sandbox rule and not by the workspace boundary.
struct Scratch {
    root: PathBuf,
    ws: PathBuf,
    proj: PathBuf,
    /// The submodule's git dir: `<proj>/.git/modules/sub`.
    subgit: PathBuf,
    /// A nested submodule's git dir with **no** working tree
    /// (`<nest>/.git/modules/a/modules/b`). Discovery cannot reach it, so only the
    /// nested-module regex guards it.
    nestedgit: PathBuf,
    /// The host repository of `wt-in`, whose `.git` is the worktree's shared common dir.
    host: PathBuf,
    /// The linked worktree of `host`, inside the workspace.
    wtin_name: String,
}

impl Scratch {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("radian-git-writable-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // The workspace repository, with an untracked file and a real (non-executable) hook to
        // overwrite. The hook is not executable, so `git commit` never runs it.
        let proj = ws.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        init_repo(&proj, "readme.md");
        std::fs::write(proj.join("new.txt"), "new\n").unwrap();
        std::fs::write(proj.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();

        // A real submodule. `protocol.file.allow` is needed because the source is a local path.
        let subsrc = root.join("subsrc");
        std::fs::create_dir_all(&subsrc).unwrap();
        init_repo(&subsrc, "s.txt");
        git(
            &proj,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                subsrc.to_str().unwrap(),
                "sub",
            ],
        );
        let subgit = std::fs::canonicalize(proj.join(".git/modules/sub")).unwrap();

        // A separate repository with a two-level submodule chain whose working trees are gone
        // (as after `git submodule deinit`): `nest/.git/modules/a/modules/b` exists, but neither
        // `nest/a/.git` nor `nest/a/b/.git` does, so discovery cannot reach the nested git dir.
        // Only the nested-module regex on `nest/.git` protects `modules/a/modules/b/{config,
        // hooks}` — which is exactly what a one-level (non-nested) regex would miss.
        let nest = ws.join("nest");
        std::fs::create_dir_all(&nest).unwrap();
        init_repo(&nest, "n.txt");
        let asrc = root.join("asrc");
        std::fs::create_dir_all(&asrc).unwrap();
        init_repo(&asrc, "a.txt");
        git(
            &nest,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                asrc.to_str().unwrap(),
                "a",
            ],
        );
        let bsrc = root.join("bsrc");
        std::fs::create_dir_all(&bsrc).unwrap();
        init_repo(&bsrc, "b.txt");
        git(
            &nest.join("a"),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                bsrc.to_str().unwrap(),
                "b",
            ],
        );
        let nestedgit = std::fs::canonicalize(nest.join(".git/modules/a/modules/b")).unwrap();
        std::fs::remove_dir_all(nest.join("a/b")).unwrap();
        std::fs::remove_dir_all(nest.join("a")).unwrap();
        // `git init` creates `hooks/`; make sure it exists so the control can write a file in it.
        std::fs::create_dir_all(nestedgit.join("hooks")).unwrap();

        // A second repository with a linked worktree, both inside the workspace.
        let host = ws.join("wthost");
        std::fs::create_dir_all(&host).unwrap();
        init_repo(&host, "h.txt");
        let wtin = ws.join("wt-in");
        git(
            &host,
            &[
                "worktree",
                "add",
                "-q",
                wtin.to_str().unwrap(),
                "-b",
                "inner",
            ],
        );
        let wtin_name = "wt-in".to_string();

        Self {
            root,
            ws,
            proj,
            subgit,
            nestedgit,
            host,
            wtin_name,
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn init_repo(dir: &Path, file: &str) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "sandbox@example.com"]);
    git(dir, &["config", "user.name", "sandbox"]);
    std::fs::write(dir.join(file), "hello\n").unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", "init"]);
}

/// Runs `script` under `sandbox` and reports whether it exited 0.
fn run(sandbox: &Sandbox, script: &str) -> bool {
    match sandbox.command("/bin/sh", ["-c", script]) {
        Ok(mut command) => command
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false),
        Err(e) => panic!("the sandbox could not build a command: {e}"),
    }
}

/// Asserts the sandbox behaves as expected **and** the control confirms the script itself is
/// sound: a denial passes only if the same script succeeds unsandboxed.
fn expect(sandbox: &Sandbox, control: &Sandbox, name: &str, must_succeed: bool, script: &str) {
    let sb = run(sandbox, script);
    let ct = run(control, script);
    if must_succeed {
        assert!(sb, "{name}: the sandbox refused a permitted command");
        assert!(ct, "{name}: the control failed; the script is not sound");
    } else {
        assert!(!sb, "{name}: the sandbox allowed a write it must deny");
        assert!(ct, "{name}: the control failed; the denial proves nothing");
    }
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// A writable sandbox over `scratch.ws` with the given git access.
fn sandbox(scratch: &Scratch, access: GitAccess) -> Sandbox {
    let policy = SandboxPolicy::workspace_write(&scratch.ws)
        .unwrap()
        .git_access(access);
    Sandbox::from_policy(policy).expect("the macOS backend must probe")
}

#[test]
fn writable_except_exec_allows_git_and_denies_the_exec_entries() {
    let scratch = Scratch::new();
    let control = Sandbox::new(SandboxMode::FullAccess, &scratch.ws).unwrap();
    let git_writable = sandbox(&scratch, GitAccess::WritableExceptExec);
    let proj = &scratch.proj;
    let git = proj.join(".git");

    // Denied: the entries git executes. `config` is written through git so the control leaves a
    // valid config behind (a raw append would corrupt it for later cases).
    expect(
        &git_writable,
        &control,
        "repo_config",
        false,
        &format!("cd {} && git config core.sbx-harness 1", q(proj)),
    );
    expect(
        &git_writable,
        &control,
        "repo_config_worktree_new",
        false,
        &format!("printf x > {}", q(&git.join("config.worktree"))),
    );
    expect(
        &git_writable,
        &control,
        "repo_hook_new_file",
        false,
        &format!("printf x > {}", q(&git.join("hooks/sbx-new-hook"))),
    );
    expect(
        &git_writable,
        &control,
        "repo_hook_overwrite",
        false,
        &format!("printf x > {}", q(&git.join("hooks/pre-commit"))),
    );

    // Denied: a submodule's git dir (config + hooks).
    expect(
        &git_writable,
        &control,
        "submodule_config",
        false,
        &format!("cd {} && git config core.sbx-harness 1", q(&scratch.subgit)),
    );
    expect(
        &git_writable,
        &control,
        "submodule_hook",
        false,
        &format!(
            "printf x > {}",
            q(&scratch.subgit.join("hooks/sbx-new-hook"))
        ),
    );

    // Denied: a worktree's shared common dir (inside the workspace) and its per-worktree git
    // dir. `wthost/.git` is the common dir; `worktrees/<name>` is the per-worktree git dir.
    let common = scratch.host.join(".git");
    expect(
        &git_writable,
        &control,
        "worktree_shared_config",
        false,
        &format!("cd {} && git config core.sbx-harness 1", q(&scratch.host)),
    );
    expect(
        &git_writable,
        &control,
        "worktree_shared_hook",
        false,
        &format!("printf x > {}", q(&common.join("hooks/sbx-new-hook"))),
    );
    expect(
        &git_writable,
        &control,
        "worktree_per_gitdir_config_worktree",
        false,
        &format!(
            "printf x > {}",
            q(&common
                .join("worktrees")
                .join(&scratch.wtin_name)
                .join("config.worktree"))
        ),
    );

    // Allowed: everything else under `.git`, including `info/` (ADR 0038 excludes it on
    // purpose) and the object store.
    expect(
        &git_writable,
        &control,
        "repo_info_writable",
        true,
        &format!("printf x > {}", q(&git.join("info/sbx-info"))),
    );
    expect(
        &git_writable,
        &control,
        "repo_objects_writable",
        true,
        &format!("printf x > {}", q(&git.join("objects/sbx-object"))),
    );

    // Allowed: native git in the sandbox: add, commit, checkout -b, stash, branch -D. The test
    // identity is passed on the command line, and no executable hook exists.
    let identity = "-c user.name=Sandbox -c user.email=sandbox@example.com";
    expect(
        &git_writable,
        &control,
        "git_add",
        true,
        &format!("cd {} && git add new.txt", q(proj)),
    );
    expect(
        &git_writable,
        &control,
        "git_commit",
        true,
        &format!(
            "cd {p} && git {identity} commit --allow-empty -qm sandbox-writable",
            p = q(proj)
        ),
    );
    expect(
        &git_writable,
        &control,
        "git_checkout_branch",
        true,
        &format!(
            "cd {p} && git checkout -b sbx-checkout && git checkout - && git branch -D sbx-checkout",
            p = q(proj)
        ),
    );
    expect(
        &git_writable,
        &control,
        "git_stash",
        true,
        &format!(
            "cd {p} && printf 'x\\n' >> readme.md && git stash",
            p = q(proj)
        ),
    );
    expect(
        &git_writable,
        &control,
        "git_branch_delete",
        true,
        &format!(
            "cd {p} && git branch sbx-tmp && git branch -D sbx-tmp",
            p = q(proj)
        ),
    );
}

/// A linked worktree and a nested submodule add exec entries beyond the plain `.git`, and they
/// must stay denied too. A worktree's `.git` is a pointer **file**: writing it would repoint git
/// at a gitdir the model controls (whose hooks then run outside the sandbox), so it keeps the
/// full deny. The shared common dir's `worktrees/<name>/{gitdir,commondir,config.worktree}` are
/// the same kind of handle. A nested submodule's config is `modules/<a>/modules/<b>/config`.
/// Native git inside the worktree must still commit.
#[test]
fn writable_except_exec_protects_worktree_links_and_nested_submodules() {
    let scratch = Scratch::new();
    let control = Sandbox::new(SandboxMode::FullAccess, &scratch.ws).unwrap();
    let git_writable = sandbox(&scratch, GitAccess::WritableExceptExec);
    let wt_in = scratch.ws.join("wt-in");

    // Native git inside the linked worktree still works: its per-worktree git dir and the
    // shared object store are writable; only the executable entries are denied.
    expect(
        &git_writable,
        &control,
        "worktree_git_commit",
        true,
        &format!(
            "cd {p} && printf 'w\\n' >> h.txt && git -c user.name=Sandbox -c user.email=sandbox@example.com commit -aqm wt",
            p = q(&wt_in)
        ),
    );

    // The worktree's `.git` is a pointer FILE. Repointing it at a gitdir the model controls
    // would run that gitdir's hooks outside the sandbox, so it stays fully read-only.
    expect(
        &git_writable,
        &control,
        "worktree_pointer_file",
        false,
        &format!("printf x > {}", q(&wt_in.join(".git"))),
    );

    // The shared common dir's per-worktree link files and per-worktree config.
    let per = scratch.host.join(".git/worktrees").join(&scratch.wtin_name);
    expect(
        &git_writable,
        &control,
        "worktree_gitdir",
        false,
        &format!("printf x > {}", q(&per.join("gitdir"))),
    );
    expect(
        &git_writable,
        &control,
        "worktree_commondir",
        false,
        &format!("printf x > {}", q(&per.join("commondir"))),
    );
    expect(
        &git_writable,
        &control,
        "worktree_config_worktree",
        false,
        &format!("printf x > {}", q(&per.join("config.worktree"))),
    );

    // A nested submodule's git dir with no working tree of its own (`nest/.git/modules/a/
    // modules/b`), so discovery cannot reach it: only the two-level nested-module regex on
    // `nest/.git` guards `modules/a/modules/b/{config,hooks}`.
    expect(
        &git_writable,
        &control,
        "nested_submodule_config",
        false,
        &format!("printf x > {}", q(&scratch.nestedgit.join("config"))),
    );
    expect(
        &git_writable,
        &control,
        "nested_submodule_hook",
        false,
        &format!(
            "printf x > {}",
            q(&scratch.nestedgit.join("hooks/sbx-new-hook"))
        ),
    );
}

/// The same workspace under the task-agent rule (`ReadOnly`, the default): the whole `.git` is
/// read-only, including `info/`, and native git cannot write. This is what the escape matrix's
/// `git_readonly` group asserts; it is repeated here so the two rules are contrasted on one
/// workspace.
#[test]
fn readonly_keeps_the_whole_git_read_only() {
    let scratch = Scratch::new();
    let control = Sandbox::new(SandboxMode::FullAccess, &scratch.ws).unwrap();
    let read_only = sandbox(&scratch, GitAccess::ReadOnly);
    let git = scratch.proj.join(".git");

    expect(
        &read_only,
        &control,
        "info_denied",
        false,
        &format!("printf x > {}", q(&git.join("info/sbx-info"))),
    );
    expect(
        &read_only,
        &control,
        "hook_denied",
        false,
        &format!("printf x > {}", q(&git.join("hooks/sbx-new-hook"))),
    );
    expect(
        &read_only,
        &control,
        "git_commit_denied",
        false,
        &format!(
            "cd {p} && git -c user.name=Sandbox -c user.email=sandbox@example.com commit --allow-empty -qm x",
            p = q(&scratch.proj)
        ),
    );
    // Read-only git still works.
    expect(
        &read_only,
        &control,
        "git_status_ok",
        true,
        &format!(
            "cd {} && git --no-optional-locks status --porcelain",
            q(&scratch.proj)
        ),
    );
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git is required for this test");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

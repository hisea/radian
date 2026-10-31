//! ADR 0039 decision 4: the two memory scope directories are the file tools' one writable
//! place outside the workspace, in both tiers. `read`, `grep`, `glob`, `write` and `patch`
//! accept an absolute or `~` path under `<memory root>/global/` and — when the workspace maps
//! to a project — `<memory root>/projects/<ProjectId>/`; everything else under `memory/` stays
//! the sandbox's deny set, and the sandbox (the shell) has no exception at all.
//!
//! Every fixture is its own `TempDir` (no process-wide environment): the tools are built with
//! `CodingTools::with_roots_and_deny` (the `workspace` tier) or
//! `CodingTools::with_sandbox_slot_and_roots` (a `FullAccess` slot, the `full` tier), passing
//! exactly the memory directories the host's `MemoryService` would pass.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};

use common::{TempDir, call, text};
use radian_coding::{CodingTools, DenySet, MemoryRoots, ReadRoots};
use radian_sandbox::{Sandbox, SandboxMode, SandboxSlot};
use serde_json::json;

/// The project the fixture workspace maps to, and a project it does not.
const PROJECT: &str = "P";
const OTHER_PROJECT: &str = "Q";

struct Fixture {
    /// Owns the scratch tree until the test ends (never read again).
    #[allow(dead_code)]
    dir: TempDir,
    workspace: PathBuf,
    memory: PathBuf,
    global: PathBuf,
    project: PathBuf,
    other_project: PathBuf,
    credentials: PathBuf,
    /// A file outside every memory scope, the symlink target.
    outside: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let workspace = dir.file("ws");
        let memory = dir.file("radian/memory");
        let global = memory.join("global");
        let project = memory.join("projects").join(PROJECT);
        let other_project = memory.join("projects").join(OTHER_PROJECT);
        let credentials = memory.join("credentials");
        let outside = dir.file("outside");
        for path in [
            &workspace,
            &global,
            &project,
            &other_project,
            &credentials,
            &outside,
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(global.join("g.md"), "global line\n").unwrap();
        std::fs::write(project.join("existing.md"), "one\ntwo\n").unwrap();
        std::fs::write(other_project.join("secret.md"), "OTHER-PROJECT\n").unwrap();
        std::fs::write(memory.join("projects.jsonl"), "{\"key\":\"x\"}\n").unwrap();
        std::fs::write(credentials.join("x"), "CREDENTIAL-SECRET\n").unwrap();
        std::fs::write(outside.join("target.txt"), "OUTSIDE\n").unwrap();
        Self {
            dir,
            workspace,
            memory,
            global,
            project,
            other_project,
            credentials,
            outside,
        }
    }

    fn scopes(&self) -> [PathBuf; 2] {
        [self.global.clone(), self.project.clone()]
    }

    /// The `workspace` tier: no tier source, so outside paths outside the roots refuse.
    fn workspace_tools(&self) -> CodingTools {
        CodingTools::with_roots_and_deny(
            &self.workspace,
            ReadRoots::none(),
            MemoryRoots::from_dirs(&self.scopes()),
            self.deny(&self.scopes()),
        )
        .unwrap()
    }

    /// The `full` tier: a `FullAccess` slot and the host's deny set, with the two scopes as
    /// its documented exception.
    fn full_tools(&self) -> CodingTools {
        let slot = SandboxSlot::with(
            Sandbox::new(SandboxMode::FullAccess, &self.workspace).expect("full access"),
        );
        CodingTools::with_sandbox_slot_and_roots(
            &self.workspace,
            slot,
            ReadRoots::none(),
            MemoryRoots::from_dirs(&self.scopes()),
            self.deny(&self.scopes()),
        )
        .unwrap()
    }

    /// The `full` tier whose only memory scope is `global/`: the workspace maps to no project.
    fn global_only_full_tools(&self) -> CodingTools {
        let slot = SandboxSlot::with(
            Sandbox::new(SandboxMode::FullAccess, &self.workspace).expect("full access"),
        );
        CodingTools::with_sandbox_slot_and_roots(
            &self.workspace,
            slot,
            ReadRoots::none(),
            MemoryRoots::from_dirs(std::slice::from_ref(&self.global)),
            self.deny(std::slice::from_ref(&self.global)),
        )
        .unwrap()
    }

    fn deny(&self, scopes: &[PathBuf]) -> DenySet {
        DenySet::from_home_and_extra(None, std::slice::from_ref(&self.memory)).allowing(scopes)
    }
}

fn abs(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn assert_outside(failure: &radian_core::ToolFailure, what: &str) {
    assert_eq!(
        failure.code, "path_outside_workspace",
        "expected a boundary refusal for {what}, got {failure:?}"
    );
}

// ---------- 1: write then read, both tiers, mode 0600 ----------

#[tokio::test]
async fn a_project_memory_file_is_written_0600_and_read_back_in_both_tiers() {
    let fx = Fixture::new("memory-roots-write-read");
    for (tier, tools, name) in [
        ("workspace", fx.workspace_tools(), "w.md"),
        ("full", fx.full_tools(), "f.md"),
    ] {
        let target = fx.project.join(name);
        let out = call(
            &tools,
            "write",
            json!({"path": abs(&target), "content": "MEMORY-BYTES\n"}),
        )
        .await
        .unwrap_or_else(|failure| panic!("{tier}: write {target:?}: {failure:?}"));
        assert!(text(&out).contains("wrote"), "{tier}: {}", text(&out));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{tier}: a memory file is owner-only");

        let out = call(&tools, "read", json!({"path": abs(&target)}))
            .await
            .unwrap_or_else(|failure| panic!("{tier}: read {target:?}: {failure:?}"));
        assert_eq!(text(&out), "1: MEMORY-BYTES\n", "{tier}");
    }
}

// ---------- 2: patch an existing memory file ----------

#[tokio::test]
async fn patch_applies_to_an_existing_memory_file_in_the_workspace_tier() {
    let fx = Fixture::new("memory-roots-patch");
    let tools = fx.workspace_tools();
    let target = fx.project.join("existing.md");
    let out = call(
        &tools,
        "patch",
        json!({"path": abs(&target), "old": "one", "new": "ONE"}),
    )
    .await
    .expect("patch a memory file");
    assert!(text(&out).contains("replaced 1"), "{}", text(&out));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "ONE\ntwo\n",
        "the patch was applied in place"
    );
}

// ---------- 3: credentials and the registry keep the deny text ----------

#[tokio::test]
async fn credentials_and_the_registry_under_memory_are_denied_in_the_full_tier() {
    let fx = Fixture::new("memory-roots-deny");
    let tools = fx.full_tools();

    for path in [fx.credentials.join("x"), fx.memory.join("projects.jsonl")] {
        let failure = call(&tools, "read", json!({"path": abs(&path)}))
            .await
            .expect_err("a path under memory outside the scopes is denied");
        assert_outside(&failure, &abs(&path));
        assert!(
            failure.message.contains("deny set"),
            "the refusal keeps the sandbox's deny text: {failure:?}"
        );
    }
}

// ---------- 4: another project's directory is not a root ----------

#[tokio::test]
async fn another_projects_directory_is_refused_in_both_tiers() {
    let fx = Fixture::new("memory-roots-other-project");
    let target = fx.other_project.join("secret.md");

    let failure = call(&fx.full_tools(), "read", json!({"path": abs(&target)}))
        .await
        .expect_err("another project's directory is not a scope");
    assert_outside(&failure, &abs(&target));
    assert!(
        failure.message.contains("deny set"),
        "the full tier refuses it through the deny set: {failure:?}"
    );

    let failure = call(&fx.workspace_tools(), "read", json!({"path": abs(&target)}))
        .await
        .expect_err("the workspace tier refuses it too");
    assert_outside(&failure, &abs(&target));
}

// ---------- 5: a symlink in a scope is never followed ----------

#[tokio::test]
async fn a_symlink_in_a_memory_scope_is_refused_for_read_and_write() {
    let fx = Fixture::new("memory-roots-symlink");
    let link = fx.global.join("link.md");
    std::os::unix::fs::symlink(fx.outside.join("target.txt"), &link).unwrap();
    let tools = fx.workspace_tools();

    let failure = call(&tools, "read", json!({"path": abs(&link)}))
        .await
        .expect_err("read refuses the link");
    assert_outside(&failure, &abs(&link));

    let failure = call(
        &tools,
        "write",
        json!({"path": abs(&link), "content": "REPLACED\n"}),
    )
    .await
    .expect_err("write refuses the link");
    assert_outside(&failure, &abs(&link));
    assert_eq!(
        std::fs::read_to_string(fx.outside.join("target.txt")).unwrap(),
        "OUTSIDE\n",
        "the link's target was never written"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself is untouched"
    );
}

// ---------- 6: `..` is refused before any root is consulted ----------

#[tokio::test]
async fn a_parent_dir_component_in_a_memory_path_is_refused() {
    let fx = Fixture::new("memory-roots-parent");
    let path = format!("{}/global/../credentials/x", fx.memory.display());
    let tools = fx.full_tools();

    let failure = call(&tools, "read", json!({"path": path}))
        .await
        .expect_err("read refuses `..`");
    assert_outside(&failure, &path);
    assert!(
        failure.message.contains("`..` is refused"),
        "the `..` refusal comes before any root: {failure:?}"
    );

    let failure = call(&tools, "write", json!({"path": path, "content": "x"}))
        .await
        .expect_err("write refuses `..`");
    assert_outside(&failure, &path);
    assert_eq!(
        std::fs::read_to_string(fx.credentials.join("x")).unwrap(),
        "CREDENTIAL-SECRET\n",
        "the escaped target is untouched"
    );
}

// ---------- 7: a scope is flat; write creates no subdirectory ----------

#[tokio::test]
async fn write_does_not_create_a_subdirectory_in_a_memory_scope() {
    let fx = Fixture::new("memory-roots-flat");
    let target = fx.global.join("sub/x.md");
    let failure = call(
        &fx.workspace_tools(),
        "write",
        json!({"path": abs(&target), "content": "nested\n"}),
    )
    .await
    .expect_err("a scope holds no subdirectory");
    assert_eq!(failure.code, "memory_scope_flat", "{failure:?}");
    assert!(
        !fx.global.join("sub").exists(),
        "no subdirectory was created"
    );
    assert!(!target.exists(), "no file was created");
}

// ---------- 8: no project mapped: global is a root, projects/ is not ----------

#[tokio::test]
async fn an_unlinked_workspace_has_the_global_scope_only() {
    let fx = Fixture::new("memory-roots-global-only");
    let tools = fx.global_only_full_tools();

    let out = call(
        &tools,
        "read",
        json!({"path": abs(&fx.global.join("g.md"))}),
    )
    .await
    .expect("global/ is a root");
    assert_eq!(text(&out), "1: global line\n");

    let failure = call(
        &tools,
        "read",
        json!({"path": abs(&fx.project.join("existing.md"))}),
    )
    .await
    .expect_err("projects/<id>/ is not a root when no project is mapped");
    assert_outside(&failure, &abs(&fx.project.join("existing.md")));
    assert!(failure.message.contains("deny set"), "{failure:?}");
}

// ---------- 9: grep and glob results carry the root prefix ----------

#[tokio::test]
async fn grep_and_glob_results_under_a_memory_scope_carry_the_root_prefix() {
    let fx = Fixture::new("memory-roots-search");
    std::fs::write(fx.project.join("needle.md"), "a needle line\n").unwrap();
    let tools = fx.workspace_tools();

    let out = call(
        &tools,
        "grep",
        json!({"pattern": "needle line", "path": abs(&fx.project)}),
    )
    .await
    .expect("grep searches the memory scope");
    let body = text(&out);
    assert_eq!(
        body,
        format!("{}:1:a needle line\n", abs(&fx.project.join("needle.md"))),
        "the result names the file under the scope the model passed"
    );

    let out = call(
        &tools,
        "glob",
        json!({"pattern": "*.md", "path": abs(&fx.project)}),
    )
    .await
    .expect("glob searches the memory scope");
    let body = text(&out);
    assert!(
        body.lines()
            .any(|line| line == abs(&fx.project.join("needle.md"))),
        "the glob result carries the root prefix: {body}"
    );
}

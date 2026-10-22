//! ADR 0039 decision 4: the file tools read skill directories in both tiers, and any
//! path except the deny set in the `full` tier.
//!
//! `HOME`, `CARGO_HOME` and `RUSTUP_HOME` are process-wide, so this binary sets them
//! **once** to a private tree under the system temp dir and every test shares it; the
//! developer's real home is never read. The tools are built with
//! `CodingTools::with_sandbox_slot_and_access` / `with_read_roots_and_deny`, passing the
//! skill roots and deny set the host would pass.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::{call, text};
use radian_coding::{CodingTools, DenySet, ReadRoots};
use radian_sandbox::{Sandbox, SandboxMode, SandboxSlot};
use serde_json::json;

struct Fixture {
    home: PathBuf,
    workspace: PathBuf,
    /// The trusted skill's directory (a read-only root): `<home>/skills/my-skill`.
    skill_dir: PathBuf,
    /// A file bundled with the skill: `<skill_dir>/references/notes.md`.
    skill_file: PathBuf,
    /// A sibling of the skill directory, under its parent but under no root.
    skill_sibling: PathBuf,
    /// A skill directory that is never added as a root (untrusted).
    evil_dir: PathBuf,
    evil_file: PathBuf,
    /// An ordinary file under `$HOME`, outside the workspace and every root.
    outside: PathBuf,
    /// A symlink under `$HOME` pointing into a denied directory.
    denied_link: PathBuf,
    /// The host's deny-read list (sessions, memory, credentials, trust, access).
    extras: Vec<PathBuf>,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn frontmatter(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\nBody line.\n")
}

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("radian-full-reads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let home = std::fs::canonicalize(&base).unwrap();

        // Every sandbox deny-read entry exists, so the expansion resolves all of them.
        for rel in radian_sandbox::DEFAULT_DENY_READ {
            let path = home.join(rel);
            if rel.ends_with(".netrc") || rel.ends_with(".npmrc") || rel.ends_with(".json") {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "SECRET\n").unwrap();
            } else {
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("sentinel.txt"), "SECRET\n").unwrap();
            }
        }

        // The host's `$RADIAN_HOME` deny-read list.
        let radian_home = home.join("radian-home");
        let mut extras = Vec::new();
        for name in ["sessions", "memory", "credentials", "trust", "access"] {
            let dir = radian_home.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("sentinel.txt"), "SESSION-SECRET\n").unwrap();
            extras.push(dir);
        }

        // The trusted skill and its bundled file.
        let skill_dir = home.join("skills/my-skill");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), frontmatter("my-skill", "Mine.")).unwrap();
        let skill_file = skill_dir.join("references/notes.md");
        std::fs::write(&skill_file, "skill notes line\n").unwrap();
        // A sibling under the skill parent, under no root.
        let skill_sibling = home.join("skills/other.txt");
        std::fs::write(&skill_sibling, "sibling\n").unwrap();

        // A skill directory that is never a root (an untrusted project's skill).
        let evil_dir = home.join("project/.agents/skills/evil");
        std::fs::create_dir_all(evil_dir.join("references")).unwrap();
        std::fs::write(evil_dir.join("SKILL.md"), frontmatter("evil", "Evil.")).unwrap();
        let evil_file = evil_dir.join("references/secret.md");
        std::fs::write(&evil_file, "evil secret\n").unwrap();

        // An ordinary file outside the workspace and every root.
        let outside = home.join("notes.txt");
        std::fs::write(&outside, "outside line one\n").unwrap();

        // A symlink into a denied directory.
        let denied_link = home.join("link-to-ssh");
        std::os::unix::fs::symlink(home.join(".ssh"), &denied_link).unwrap();

        let workspace = home.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        // SAFETY: this binary sets these three variables exactly once, before any tool reads
        // them. The `OnceLock` serialises the write and every reader runs after it returns.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("CARGO_HOME", home.join(".cargo"));
            std::env::set_var("RUSTUP_HOME", home.join(".rustup"));
        }

        Fixture {
            home,
            workspace,
            skill_dir,
            skill_file,
            skill_sibling,
            evil_dir,
            evil_file,
            outside,
            denied_link,
            extras,
        }
    })
}

fn full_sandbox(workspace: &Path) -> Sandbox {
    Sandbox::new(SandboxMode::FullAccess, workspace).expect("full-access needs no probe")
}

/// The `full`-tier tools: a slot holding a `FullAccess` sandbox, the skill root, and the
/// deny set.
fn full_tools() -> (CodingTools, SandboxSlot) {
    let fx = fixture();
    let slot = SandboxSlot::with(full_sandbox(&fx.workspace));
    let tools = CodingTools::with_sandbox_slot_and_access(
        &fx.workspace,
        slot.clone(),
        ReadRoots::from_env_and_skills(std::slice::from_ref(&fx.skill_dir)),
        DenySet::from_home_and_extra(Some(fx.home.clone()), &fx.extras),
    )
    .unwrap();
    (tools, slot)
}

/// The `workspace`-tier tools: no tier source, so outside paths outside the roots refuse.
fn workspace_tools() -> CodingTools {
    let fx = fixture();
    CodingTools::with_read_roots_and_deny(
        &fx.workspace,
        ReadRoots::from_env_and_skills(std::slice::from_ref(&fx.skill_dir)),
        DenySet::from_home_and_extra(Some(fx.home.clone()), &fx.extras),
    )
    .unwrap()
}

fn abs(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------- skill roots: both tiers ----------

#[tokio::test]
async fn a_skill_bundled_file_reads_through_read_in_both_tiers() {
    let fx = fixture();
    let (full, _) = full_tools();
    let out = call(&full, "read", json!({"path": abs(&fx.skill_file)}))
        .await
        .expect("a skill file reads in the full tier");
    assert!(text(&out).contains("skill notes line"), "{}", text(&out));

    let workspace = workspace_tools();
    let out = call(&workspace, "read", json!({"path": abs(&fx.skill_file)}))
        .await
        .expect("a skill file reads in the workspace tier");
    assert!(text(&out).contains("skill notes line"), "{}", text(&out));
}

#[tokio::test]
async fn a_skill_directory_is_searchable_through_grep_and_glob_in_both_tiers() {
    let fx = fixture();
    let (full, _) = full_tools();
    let out = call(
        &full,
        "grep",
        json!({"pattern": "skill notes", "path": abs(&fx.skill_dir)}),
    )
    .await
    .expect("grep searches the skill directory");
    let body = text(&out);
    assert!(body.contains(&abs(&fx.skill_file)), "{body}");
    let workspace = workspace_tools();
    let out = call(
        &workspace,
        "grep",
        json!({"pattern": "skill notes", "path": abs(&fx.skill_dir)}),
    )
    .await
    .expect("grep searches the skill directory in the workspace tier");
    assert!(text(&out).contains(&abs(&fx.skill_file)), "{}", text(&out));

    for (tools, tier) in [(&full, "full"), (&workspace, "workspace")] {
        let out = call(
            tools,
            "glob",
            json!({"pattern": "**/*.md", "path": abs(&fx.skill_dir)}),
        )
        .await
        .unwrap_or_else(|_| panic!("glob searches the skill directory in the {tier} tier"));
        let body = text(&out);
        assert!(body.contains(&abs(&fx.skill_file)), "{tier}: {body}");
    }
}

#[tokio::test]
async fn a_skill_directory_lists_through_read_in_both_tiers() {
    let fx = fixture();
    let (full, _) = full_tools();
    let out = call(&full, "read", json!({"path": abs(&fx.skill_dir)}))
        .await
        .expect("a skill directory lists in the full tier");
    assert!(text(&out).contains("SKILL.md"), "{}", text(&out));

    let workspace = workspace_tools();
    let out = call(&workspace, "read", json!({"path": abs(&fx.skill_dir)}))
        .await
        .expect("a skill directory lists in the workspace tier");
    assert!(text(&out).contains("SKILL.md"), "{}", text(&out));
}

// ---------- not roots: the skill parent, an untrusted skill dir ----------

#[tokio::test]
async fn a_skill_parent_is_not_a_root() {
    let fx = fixture();
    let workspace = workspace_tools();
    // The parent holds the skill but is itself under no root: a file under it but outside
    // the skill directory refuses in the `workspace` tier.
    let failure = call(&workspace, "read", json!({"path": abs(&fx.skill_sibling)}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");

    let failure = call(
        &workspace,
        "grep",
        json!({"pattern": "sibling", "path": abs(fx.skill_dir.parent().unwrap())}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");

    let failure = call(
        &workspace,
        "glob",
        json!({"pattern": "*.txt", "path": abs(fx.skill_dir.parent().unwrap())}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
}

#[tokio::test]
async fn an_untrusted_skill_dir_is_not_a_root() {
    let fx = fixture();
    let workspace = workspace_tools();
    // The directory was never added as a root (its project is untrusted), so an absolute
    // path under it refuses in the `workspace` tier.
    let failure = call(&workspace, "read", json!({"path": abs(&fx.evil_file)}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        failure.message.contains(&abs(&fx.skill_dir)),
        "the refusal lists the skill root that *is* readable: {}",
        failure.message
    );

    let failure = call(
        &workspace,
        "grep",
        json!({"pattern": "evil", "path": abs(&fx.evil_dir)}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
}

// ---------- full tier: free reads ----------

#[tokio::test]
async fn an_outside_path_under_home_reads_in_the_full_tier() {
    let fx = fixture();
    let (tools, _) = full_tools();
    for path in [abs(&fx.outside), "~/notes.txt".to_string()] {
        let out = call(&tools, "read", json!({"path": path}))
            .await
            .unwrap_or_else(|_| panic!("{path} reads in the full tier"));
        assert!(
            text(&out).contains("outside line one"),
            "{path}: {}",
            text(&out)
        );
    }

    let out = call(
        &tools,
        "grep",
        json!({"pattern": "outside line", "path": "~/"}),
    )
    .await
    .expect("grep searches an outside directory in the full tier");
    assert!(text(&out).contains("outside line one"), "{}", text(&out));

    let out = call(
        &tools,
        "glob",
        json!({"pattern": "notes.txt", "path": "~/"}),
    )
    .await
    .expect("glob searches an outside directory in the full tier");
    assert!(text(&out).contains("notes.txt"), "{}", text(&out));
}

// ---------- full tier: the deny set ----------

/// Every sandbox deny-read entry refuses through the file tools, naming the sandbox's deny
/// set — never the file content.
#[tokio::test]
async fn every_default_deny_read_entry_refuses() {
    let fx = fixture();
    let (tools, _) = full_tools();
    for rel in radian_sandbox::DEFAULT_DENY_READ {
        // Read the entry itself when it is a file, else the sentinel inside it.
        let entry = fx.home.join(rel);
        let target = if entry.is_file() {
            entry.clone()
        } else {
            entry.join("sentinel.txt")
        };
        for path in [abs(&target)] {
            let failure = call(&tools, "read", json!({"path": path}))
                .await
                .unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{path}: {failure:?}"
            );
            assert!(
                failure.message.contains("sandbox's deny set"),
                "{path}: the refusal names the sandbox's deny set: {}",
                failure.message
            );
            assert!(
                !failure.message.contains("SECRET"),
                "{path}: the message leaked content: {}",
                failure.message
            );
        }
    }
}

#[tokio::test]
async fn every_denied_radian_home_path_refuses() {
    let fx = fixture();
    let (tools, _) = full_tools();
    for dir in &fx.extras {
        let target = dir.join("sentinel.txt");
        let failure = call(&tools, "read", json!({"path": abs(&target)}))
            .await
            .unwrap_err();
        assert_eq!(
            failure.code,
            "path_outside_workspace",
            "{}: {failure:?}",
            target.display()
        );
        assert!(
            failure.message.contains("sandbox's deny set") && failure.message.contains(&abs(dir)),
            "the refusal names the denied path and the deny set: {}",
            failure.message
        );
        assert!(
            !failure.message.contains("SESSION-SECRET"),
            "no content leaks: {}",
            failure.message
        );

        let failure = call(
            &tools,
            "grep",
            json!({"pattern": "SESSION", "path": abs(dir)}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");

        let failure = call(&tools, "glob", json!({"pattern": "**/*", "path": abs(dir)}))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    }
}

#[tokio::test]
async fn a_symlink_pointing_into_a_denied_directory_refuses() {
    let fx = fixture();
    let (tools, _) = full_tools();
    // Through the link: `realpath` lands inside the deny set.
    let failure = call(
        &tools,
        "read",
        json!({"path": abs(&fx.denied_link.join("sentinel.txt"))}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        failure.message.contains("sandbox's deny set"),
        "the refusal names the deny set: {}",
        failure.message
    );
    assert!(
        !failure.message.contains("SECRET"),
        "no content leaks: {}",
        failure.message
    );
}

// ---------- writes never leave the workspace ----------

#[tokio::test]
async fn write_and_patch_outside_the_workspace_refuse_in_the_full_tier() {
    let fx = fixture();
    let (tools, _) = full_tools();
    let target = fx.home.join("pwned.txt");
    let failure = call(
        &tools,
        "write",
        json!({"path": abs(&target), "content": "PWNED"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(!target.exists(), "nothing was written");

    let failure = call(
        &tools,
        "patch",
        json!({"path": abs(&fx.outside), "old": "outside", "new": "pwned"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        std::fs::read_to_string(&fx.outside)
            .unwrap()
            .contains("outside line one"),
        "the file is untouched"
    );
}

// ---------- workspace tier: the same path refuses and lists roots ----------

#[tokio::test]
async fn the_workspace_tier_refuses_the_same_outside_path_and_lists_the_roots() {
    let fx = fixture();
    let tools = workspace_tools();
    let failure = call(&tools, "read", json!({"path": abs(&fx.outside)}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        failure.message.contains("absolute path") && failure.message.contains(&abs(&fx.outside)),
        "the refusal still names the path kind and the path: {}",
        failure.message
    );
    assert!(
        failure.message.contains("$CARGO_HOME/registry/src"),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains(&abs(&fx.skill_dir)),
        "the refusal lists the skill root: {}",
        failure.message
    );

    let failure = call(
        &tools,
        "grep",
        json!({"pattern": "outside", "path": abs(&fx.outside)}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
}

// ---------- a tier switch changes the next call ----------

#[tokio::test]
async fn a_tier_switch_changes_what_the_file_tools_read_at_the_next_call() {
    let fx = fixture();
    let (tools, slot) = full_tools();
    let path = abs(&fx.outside);

    call(&tools, "read", json!({"path": path}))
        .await
        .expect("the full tier reads the outside path");

    // `full` → `workspace` (the slot now holds nothing): the next call refuses.
    slot.set(None);
    let failure = call(&tools, "read", json!({"path": path}))
        .await
        .unwrap_err();
    assert_eq!(
        failure.code, "path_outside_workspace",
        "the switch takes effect at the next call: {failure:?}"
    );

    // And back: the next call reads again.
    slot.set(Some(full_sandbox(&fx.workspace)));
    call(&tools, "read", json!({"path": path}))
        .await
        .expect("switching back to full reads again");
}

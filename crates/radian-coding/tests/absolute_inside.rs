//! ADR 0039 decision 6: an absolute path under the workspace root, in either spelling of the
//! root, is the workspace-relative path. It goes through exactly the checks a relative path
//! gets (no `..`, the fd walk, the symlink rule, relative output naming); everything else is
//! unchanged.
//!
//! Every fixture builds the workspace through a symlinked parent, the shape `/tmp` and
//! `/private/tmp` have on macOS: `given` is the path the tools were built with, `canonical`
//! what `root()` reports. A tool must accept both.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};

use common::{TempDir, call, text};
use radian_coding::{CodingTools, DenySet, ReadRoots};
use radian_sandbox::{Sandbox, SandboxMode, SandboxSlot};
use serde_json::json;

struct Fixture {
    dir: TempDir,
    /// The path the tools are built with (through the `link` symlink).
    given: PathBuf,
    /// What the tools canonicalize it to.
    canonical: PathBuf,
}

fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let real = dir.file("real");
    std::fs::create_dir_all(real.join("ws")).unwrap();
    std::os::unix::fs::symlink(&real, dir.file("link")).unwrap();
    let given = dir.file("link/ws");
    let canonical = std::fs::canonicalize(&given).unwrap();
    assert_ne!(
        given, canonical,
        "the fixture needs two different spellings"
    );
    Fixture {
        dir,
        given,
        canonical,
    }
}

impl Fixture {
    /// The absolute spelling of `relative` in the given order: built from the given root, then
    /// from the canonical one. `""` is the root itself.
    fn spellings(&self, relative: &str) -> [String; 2] {
        let join = |root: &Path| {
            let path = root.join(relative);
            path.to_string_lossy().into_owned()
        };
        [join(&self.given), join(&self.canonical)]
    }
}

fn abs(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A full-tier tool set: a `FullAccess` slot and no deny set unless one is given. The
/// sandbox canonicalizes the workspace, so the tools may be built with either spelling.
fn full_tools(root: &Path, deny: DenySet) -> CodingTools {
    let slot = SandboxSlot::with(Sandbox::new(SandboxMode::FullAccess, root).expect("full access"));
    CodingTools::with_sandbox_slot_and_access(root, slot, ReadRoots::none(), deny).unwrap()
}

// ---------- every file tool, both spellings ----------

#[tokio::test]
async fn every_file_tool_takes_an_absolute_path_under_the_root_in_both_spellings() {
    let fx = fixture("abs-all-tools");
    std::fs::create_dir_all(fx.canonical.join("sub")).unwrap();
    std::fs::write(fx.canonical.join("sub/a.txt"), "SENTINEL a\n").unwrap();
    std::fs::write(fx.canonical.join("sub/b.txt"), "plain b\n").unwrap();
    let tools = common::tools(&fx.given);
    assert_eq!(tools.root(), fx.canonical);

    for (round, spelling) in fx.spellings("sub/a.txt").into_iter().enumerate() {
        // read
        let out = call(&tools, "read", json!({"path": spelling}))
            .await
            .unwrap_or_else(|failure| panic!("read {spelling}: {failure:?}"));
        assert_eq!(text(&out), "1: SENTINEL a\n", "{spelling}");

        // write, into a missing directory: the absolute path creates it like a relative one
        let target = format!("sub/new-{round}.txt");
        let out = call(
            &tools,
            "write",
            json!({"path": fx.spellings(&target)[round], "content": "W\n"}),
        )
        .await
        .unwrap_or_else(|failure| panic!("write {target}: {failure:?}"));
        assert!(text(&out).contains("wrote 2 bytes"), "{}", text(&out));
        assert_eq!(
            std::fs::read_to_string(fx.canonical.join(&target)).unwrap(),
            "W\n"
        );

        // patch, both forms
        let target = format!("sub/patch-{round}.txt");
        std::fs::write(fx.canonical.join(&target), "one\ntwo\n").unwrap();
        let out = call(
            &tools,
            "patch",
            json!({"path": fx.spellings(&target)[round], "old": "one", "new": "ONE"}),
        )
        .await
        .unwrap_or_else(|failure| panic!("patch {target}: {failure:?}"));
        assert!(text(&out).contains("replaced 1"), "{}", text(&out));
        assert!(
            std::fs::read_to_string(fx.canonical.join(&target))
                .unwrap()
                .starts_with("ONE")
        );

        let target = format!("sub/edits-{round}.txt");
        std::fs::write(fx.canonical.join(&target), "alpha\nbeta\n").unwrap();
        let out = call(
            &tools,
            "patch",
            json!({
                "path": fx.spellings(&target)[round],
                "edits": [{"old": "alpha", "new": "ALPHA"}, {"old": "beta", "new": "BETA"}],
            }),
        )
        .await
        .unwrap_or_else(|failure| panic!("patch edits {target}: {failure:?}"));
        assert!(text(&out).contains("across 2 edits"), "{}", text(&out));
    }

    // grep and glob on a directory and on one file: the results stay workspace-relative, as
    // they are for a relative path.
    for spelling in fx.spellings("sub") {
        let out = call(
            &tools,
            "grep",
            json!({"pattern": "SENTINEL", "path": spelling}),
        )
        .await
        .unwrap_or_else(|failure| panic!("grep {spelling}: {failure:?}"));
        assert_eq!(text(&out), "sub/a.txt:1:SENTINEL a\n", "{spelling}");

        let out = call(
            &tools,
            "glob",
            json!({"pattern": "**/*.txt", "path": spelling}),
        )
        .await
        .unwrap_or_else(|failure| panic!("glob {spelling}: {failure:?}"));
        let body = text(&out);
        for expected in ["sub/a.txt", "sub/b.txt"] {
            assert!(
                body.lines().any(|line| line == expected),
                "{spelling}: {body}"
            );
        }
        assert!(
            !body.contains(&abs(&fx.canonical)) && !body.contains(&abs(&fx.given)),
            "{spelling}: the results stay workspace-relative: {body}"
        );
    }
    for spelling in fx.spellings("sub/a.txt") {
        let out = call(
            &tools,
            "grep",
            json!({"pattern": "SENTINEL", "path": spelling}),
        )
        .await
        .unwrap_or_else(|failure| panic!("grep file {spelling}: {failure:?}"));
        assert_eq!(text(&out), "sub/a.txt:1:SENTINEL a\n", "{spelling}");
        let out = call(
            &tools,
            "glob",
            json!({"pattern": "**/*.txt", "path": spelling}),
        )
        .await
        .unwrap_or_else(|failure| panic!("glob file {spelling}: {failure:?}"));
        assert_eq!(text(&out), "sub/a.txt\n", "{spelling}");
    }
}

#[tokio::test]
async fn the_root_itself_as_an_absolute_path_lists_and_searches_the_workspace() {
    let fx = fixture("abs-root-itself");
    std::fs::write(fx.canonical.join("kept.txt"), "SENTINEL kept\n").unwrap();
    let tools = common::tools(&fx.given);

    let [given, canonical] = fx.spellings("");
    for spelling in [given.clone(), canonical.clone(), format!("{given}/")] {
        let out = call(&tools, "read", json!({"path": spelling}))
            .await
            .unwrap_or_else(|failure| panic!("read {spelling}: {failure:?}"));
        let body = text(&out);
        assert!(
            body.lines().any(|line| line.ends_with("\tkept.txt")),
            "{spelling}: {body}"
        );

        let out = call(
            &tools,
            "glob",
            json!({"pattern": "**/*.txt", "path": spelling}),
        )
        .await
        .unwrap_or_else(|failure| panic!("glob {spelling}: {failure:?}"));
        assert_eq!(text(&out), "kept.txt\n", "{spelling}");

        let out = call(
            &tools,
            "grep",
            json!({"pattern": "SENTINEL", "path": spelling}),
        )
        .await
        .unwrap_or_else(|failure| panic!("grep {spelling}: {failure:?}"));
        assert_eq!(text(&out), "kept.txt:1:SENTINEL kept\n", "{spelling}");
    }
}

// ---------- the prefix test is by components ----------

#[tokio::test]
async fn a_sibling_that_only_shares_a_string_prefix_is_not_under_the_root() {
    let fx = fixture("abs-sibling");
    let given_sibling = fx.given.parent().unwrap().join("ws2");
    let canonical_sibling = fx.canonical.parent().unwrap().join("ws2");
    std::fs::create_dir_all(&canonical_sibling).unwrap();
    std::fs::write(canonical_sibling.join("secret.txt"), "SENTINEL-CONTENT").unwrap();
    let tools = common::tools(&fx.given);

    for sibling in [&given_sibling, &canonical_sibling] {
        let secret = abs(&sibling.join("secret.txt"));
        for (name, arguments) in [
            ("read", json!({"path": secret})),
            ("grep", json!({"pattern": "SENTINEL", "path": abs(sibling)})),
            ("glob", json!({"pattern": "**/*", "path": abs(sibling)})),
            ("write", json!({"path": secret, "content": "PWNED"})),
            (
                "patch",
                json!({"path": secret, "old": "SENTINEL", "new": "PWNED"}),
            ),
        ] {
            let failure = call(&tools, name, arguments).await.unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{name} on {secret}: a component-wise prefix test refuses it, got {failure:?}"
            );
            assert!(!failure.message.contains("PWNED"), "{name}: {failure:?}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(canonical_sibling.join("secret.txt")).unwrap(),
        "SENTINEL-CONTENT",
        "nothing was written to the sibling"
    );
}

// ---------- `..` is refused before any conversion ----------

#[tokio::test]
async fn an_absolute_workspace_path_with_a_parent_component_is_refused() {
    let fx = fixture("abs-dotdot");
    let _outside = common::sentinel_outside(&fx.dir);
    std::fs::create_dir_all(fx.canonical.join("sub")).unwrap();
    std::fs::write(fx.canonical.join("a.txt"), "INSIDE").unwrap();
    let tools = common::tools(&fx.given);

    let [given, canonical] = fx.spellings("");
    let escapes = [
        // Lexically inside the root after the `..` is consumed; still refused.
        format!("{given}/sub/../a.txt"),
        format!("{canonical}/sub/../a.txt"),
        // Out of the root.
        format!("{given}/../outside/sentinel.txt"),
        format!("{canonical}/../outside/sentinel.txt"),
    ];
    for path in &escapes {
        for (name, arguments) in [
            ("read", json!({"path": path})),
            ("grep", json!({"pattern": "SENTINEL", "path": path})),
            ("glob", json!({"pattern": "**/*", "path": path})),
            (
                "write",
                json!({"path": format!("{path}/new.txt"), "content": "PWNED"}),
            ),
            ("patch", json!({"path": path, "old": "one", "new": "two"})),
        ] {
            let failure = call(&tools, name, arguments).await.unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{name} on {path}: `..` is refused before any conversion, got {failure:?}"
            );
            assert!(
                !failure.message.contains("SENTINEL-CONTENT"),
                "{name}: {failure:?}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(fx.canonical.join("a.txt")).unwrap(),
        "INSIDE"
    );
}

#[tokio::test]
async fn an_absolute_path_with_a_parent_component_is_refused_in_the_full_tier() {
    let fx = fixture("abs-dotdot-full");
    let outside = common::sentinel_outside(&fx.dir);
    std::fs::create_dir_all(fx.canonical.join("sub")).unwrap();
    std::fs::write(fx.canonical.join("a.txt"), "INSIDE").unwrap();
    // The `full` tier would read both an absolute path that normalises back inside the
    // workspace and one that normalises out of it; neither may reach the outside route.
    let tools = full_tools(&fx.given, DenySet::empty());

    let [given, canonical] = fx.spellings("");
    let escapes = [
        format!("{given}/sub/../a.txt"),
        format!("{canonical}/sub/../a.txt"),
        format!("{given}/../outside/sentinel.txt"),
        format!("{canonical}/../outside/sentinel.txt"),
    ];
    for path in &escapes {
        for (name, arguments) in [
            ("read", json!({"path": path})),
            ("grep", json!({"pattern": "SENTINEL|INSIDE", "path": path})),
            ("glob", json!({"pattern": "**/*", "path": path})),
        ] {
            let failure = call(&tools, name, arguments).await.unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{name} on {path}: `..` is refused before any conversion in the full tier, \
                 got {failure:?}"
            );
            assert!(
                !failure.message.contains("SENTINEL-CONTENT")
                    && !failure.message.contains("INSIDE"),
                "{name} on {path}: the target was read: {failure:?}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
        "SENTINEL-CONTENT"
    );
}

// ---------- the workspace's own symlink rule still applies ----------

#[tokio::test]
async fn an_absolute_workspace_path_still_refuses_a_symlink_component() {
    let fx = fixture("abs-symlink");
    let outside = common::sentinel_outside(&fx.dir);
    std::os::unix::fs::symlink(&outside, fx.canonical.join("link_dir")).unwrap();
    std::fs::create_dir_all(fx.canonical.join("inside")).unwrap();
    std::fs::write(fx.canonical.join("inside/keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink("inside", fx.canonical.join("in_link")).unwrap();

    // Both a workspace-tier and a full-tier tool set: the workspace route must be taken in
    // the full tier too, so the outside link is refused instead of `realpath`ed and read.
    for full in [false, true] {
        let tools = if full {
            full_tools(&fx.given, DenySet::empty())
        } else {
            common::tools(&fx.given)
        };
        for base in fx.spellings("link_dir") {
            let file = format!("{base}/sentinel.txt");
            for (name, arguments) in [
                ("read", json!({"path": file.clone()})),
                ("grep", json!({"pattern": "SENTINEL", "path": base.clone()})),
                ("glob", json!({"pattern": "**/*", "path": base.clone()})),
                (
                    "write",
                    json!({"path": format!("{base}/new.txt"), "content": "X"}),
                ),
                (
                    "patch",
                    json!({"path": file.clone(), "old": "SENTINEL", "new": "X"}),
                ),
            ] {
                let failure = call(&tools, name, arguments).await.expect_err(&format!(
                    "{name} on {file}: the outside link must be refused"
                ));
                assert_eq!(
                    failure.code, "path_outside_workspace",
                    "{name} on {file}: {failure:?}"
                );
                assert!(
                    failure.message.contains("link_dir") && failure.message.contains("symlink"),
                    "{name}: the message names the link: {}",
                    failure.message
                );
                assert!(
                    !failure.message.contains(&abs(&outside))
                        && !failure.message.contains("SENTINEL-CONTENT"),
                    "{name}: the outside path or content leaked: {}",
                    failure.message
                );
            }
        }
        // An in-workspace link keeps today's named-target failure.
        for spelling in fx.spellings("in_link/keep.txt") {
            let failure = call(&tools, "read", json!({"path": spelling}))
                .await
                .unwrap_err();
            assert_eq!(failure.code, "path_is_symlink", "{failure:?}");
            assert!(
                failure
                    .message
                    .contains("`in_link` is a symlink to `inside`"),
                "{}",
                failure.message
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
        "SENTINEL-CONTENT",
        "nothing outside was written"
    );
}

// ---------- the outside rules are unchanged ----------

#[tokio::test]
async fn a_full_tier_read_of_an_absolute_path_outside_the_workspace_is_unchanged() {
    let fx = fixture("abs-full-outside");
    let outside = fx.dir.file("elsewhere.txt");
    std::fs::write(&outside, "outside line one\n").unwrap();

    let full = full_tools(&fx.given, DenySet::empty());
    let out = call(&full, "read", json!({"path": abs(&outside)}))
        .await
        .expect("the full tier still reads an absolute path outside the workspace");
    assert!(text(&out).contains("outside line one"), "{}", text(&out));

    // The `workspace` tier refuses it and names the readable roots, as before.
    let workspace = common::tools(&fx.given);
    let failure = call(&workspace, "read", json!({"path": abs(&outside)}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
    assert!(
        failure.message.contains("absolute path") && failure.message.contains("$CARGO_HOME"),
        "{}",
        failure.message
    );

    // `write`/`patch` still refuse outside the workspace in every tier.
    for (name, arguments) in [
        ("write", json!({"path": abs(&outside), "content": "PWNED"})),
        (
            "patch",
            json!({"path": abs(&outside), "old": "outside", "new": "PWNED"}),
        ),
    ] {
        let failure = call(&full, name, arguments).await.unwrap_err();
        assert_eq!(
            failure.code, "path_outside_workspace",
            "{name}: {failure:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "outside line one\n"
    );
}

#[tokio::test]
async fn a_deny_set_file_inside_the_workspace_behaves_as_its_relative_path() {
    let fx = fixture("abs-deny-inside");
    let denied = fx.canonical.join(".ssh");
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::write(denied.join("secret.txt"), "SESSION-SECRET\n").unwrap();

    // The deny set contains a path inside the workspace. A relative workspace path never
    // consults it, and an absolute workspace path must not take the full-tier outside route
    // where it would.
    let tools = full_tools(
        &fx.given,
        DenySet::from_home_and_extra(None, std::slice::from_ref(&denied)),
    );
    for path in [
        ".ssh/secret.txt".to_string(),
        fx.spellings(".ssh/secret.txt")[0].clone(),
        fx.spellings(".ssh/secret.txt")[1].clone(),
    ] {
        let out = call(&tools, "read", json!({"path": path}))
            .await
            .unwrap_or_else(|failure| panic!("read {path}: {failure:?}"));
        assert!(
            text(&out).contains("SESSION-SECRET"),
            "{path}: the workspace route does not consult the deny set: {}",
            text(&out)
        );
    }
}

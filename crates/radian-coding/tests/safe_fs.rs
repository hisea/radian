//! ADR 0039 decision 2: the file tools open workspace paths through the root's directory fd,
//! one `openat(O_NOFOLLOW)` per component, and never re-open a path string after a check.
//!
//! The matrix below runs every file tool against a path whose **parent** is a symlink, once
//! pointing outside the workspace and once inside, and asserts the refusal plus that the
//! outside file is neither read nor changed. The swap test installs the `resolve_hook` seam
//! (feature `testing`): between the lexical check and the first `openat` it replaces a real
//! parent directory with a link to the outside, which is exactly the window a background job
//! has; the tool must still not reach the outside file. No sleeps anywhere.

#![cfg(unix)]

mod common;

use common::{TempDir, call, sentinel_outside, text, tools};
use serde_json::{Value, json};

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Every file tool with the arguments that make it touch `path`.
fn tool_calls(path: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("read", json!({"path": path})),
        ("write", json!({"path": path, "content": "X"})),
        (
            "patch",
            json!({"path": path, "old": "SENTINEL", "new": "X"}),
        ),
        ("glob", json!({"pattern": "**/*", "path": path})),
        ("grep", json!({"pattern": "SENTINEL", "path": path})),
    ]
}

// ---------- the matrix: a symlinked parent, outside and inside ----------

#[test]
fn every_file_tool_refuses_a_parent_symlink_to_outside_and_touches_nothing_there() {
    serial(async {
        let dir = TempDir::new("fs-matrix-outside");
        let root = dir.file("ws");
        std::fs::create_dir_all(&root).unwrap();
        let outside = sentinel_outside(&dir);
        std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
        let tools = tools(&root);

        for (name, args) in tool_calls("link_dir/sentinel.txt") {
            let failure = call(&tools, name, args).await.unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{name}: expected the boundary refusal, got {failure:?}"
            );
            assert!(
                !failure.message.contains(outside.to_str().unwrap()),
                "{name}: the outside path leaked into the message: {}",
                failure.message
            );
            assert!(
                failure.message.contains("link_dir") && failure.message.contains("symlink"),
                "{name}: the message must name the link component: {}",
                failure.message
            );
        }
        // Also the directory itself as the path, for the tools that take a directory.
        for (name, args) in tool_calls("link_dir") {
            let failure = call(&tools, name, args).await.unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{name}: {failure:?}"
            );
        }
        assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");
        assert!(
            std::fs::read_dir(&outside).unwrap().count() == 1,
            "nothing was created outside"
        );
    });
}

#[test]
fn every_file_tool_refuses_a_parent_symlink_inside_and_names_the_target() {
    serial(async {
        let dir = TempDir::new("fs-matrix-inside");
        let root = dir.file("ws");
        dir.write("ws/docs/v2/guide.md", "SENTINEL inside");
        // `docs/current -> v2` (a relative link), the shape a repo commonly carries.
        std::os::unix::fs::symlink("v2", root.join("docs/current")).unwrap();
        let tools = tools(&root);

        for (name, args) in tool_calls("docs/current/guide.md") {
            let failure = call(&tools, name, args).await.unwrap_err();
            assert_eq!(
                failure.code, "path_is_symlink",
                "{name}: an inside link is refused with its own code, got {failure:?}"
            );
            assert!(
                failure
                    .message
                    .contains("`docs/current` is a symlink to `docs/v2`"),
                "{name}: the message must name the workspace-relative target: {}",
                failure.message
            );
        }
        // The target path works for every tool.
        let out = call(&tools, "read", json!({"path": "docs/v2/guide.md"}))
            .await
            .unwrap();
        assert_eq!(text(&out), "1: SENTINEL inside");
        assert_eq!(read(&root.join("docs/v2/guide.md")), "SENTINEL inside");
    });
}

#[test]
fn an_absolute_inside_link_target_is_named_relative_to_the_workspace() {
    serial(async {
        let dir = TempDir::new("fs-abs-inside");
        let root = dir.file("ws");
        dir.write("ws/real.txt", "real");
        // An absolute target through the non-canonical temp path spelling: the classification
        // must still see it as inside and name it relative to the workspace.
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
        let tools = tools(&root);

        let failure = call(&tools, "read", json!({"path": "link.txt"}))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "path_is_symlink", "{failure:?}");
        assert!(
            failure
                .message
                .contains("`link.txt` is a symlink to `real.txt`"),
            "{}",
            failure.message
        );
    });
}

#[test]
fn a_link_leaving_the_workspace_never_prints_the_outside_path() {
    serial(async {
        let dir = TempDir::new("fs-outside-message");
        let root = dir.file("ws");
        std::fs::create_dir_all(&root).unwrap();
        let outside = sentinel_outside(&dir);
        std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("leak.txt")).unwrap();
        // A relative link that climbs above the root is "outside" too, lexically.
        std::os::unix::fs::symlink("../outside/sentinel.txt", root.join("climb.txt")).unwrap();
        // A dangling link to nowhere outside: unknown counts as outside.
        std::os::unix::fs::symlink("/nonexistent-dir-xyz/file", root.join("dangling.txt")).unwrap();
        let tools = tools(&root);

        for path in ["leak.txt", "climb.txt", "dangling.txt"] {
            let failure = call(&tools, "read", json!({"path": path}))
                .await
                .unwrap_err();
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{path}: {failure:?}"
            );
            assert!(
                failure.message.contains("symlink leaving the workspace"),
                "{path}: {}",
                failure.message
            );
            assert!(
                !failure.message.contains("sentinel.txt")
                    && !failure.message.contains("nonexistent-dir-xyz"),
                "{path}: the outside path leaked: {}",
                failure.message
            );
        }
    });
}

// ---------- the deterministic swap: the window a background job has ----------

/// Installs a hook that, once, replaces `ws/<dir>` with a symlink to `outside`. It runs after
/// the tool's lexical check and before its first `openat`, i.e. exactly where a canonicalize-
/// then-open implementation would have already decided the path was fine.
fn swap_parent_once(root: &std::path::Path, dir_name: &str, outside: &std::path::Path) {
    let victim = root.join(dir_name);
    let target = outside.to_path_buf();
    let done = std::sync::atomic::AtomicBool::new(false);
    radian_coding::safe_fs::resolve_hook(Some(Box::new(move || {
        if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::fs::remove_dir_all(&victim).unwrap();
        std::os::unix::fs::symlink(&target, &victim).unwrap();
    })));
}

fn clear_hook() {
    radian_coding::safe_fs::resolve_hook(None);
    radian_coding::safe_fs::after_open_hook(None);
}

/// The hook is process-global, so every test in this file takes this lock and runs one at a
/// time. The lock is held in sync code around `block_on`, never across an `.await`.
static SWAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial<F: std::future::Future<Output = ()>>(body: F) {
    let _guard = SWAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body);
}

#[test]
fn read_cannot_reach_outside_when_the_parent_is_swapped_after_the_check() {
    serial(async {
        let dir = TempDir::new("fs-swap-read");
        let root = dir.file("ws");
        dir.write("ws/sub/sentinel.txt", "INSIDE");
        let outside = sentinel_outside(&dir);
        let tools = tools(&root);
        swap_parent_once(&root, "sub", &outside);

        let result = call(&tools, "read", json!({"path": "sub/sentinel.txt"})).await;
        clear_hook();
        let failure = result.expect_err("the swapped parent must be refused");
        assert_eq!(failure.code, "path_outside_workspace", "{failure:?}");
        // The proof: the outside sentinel's content never reached the tool output.
        assert!(!failure.message.contains("SENTINEL-CONTENT"));
    });
}

#[test]
fn write_and_patch_cannot_reach_outside_when_the_parent_is_swapped_after_the_check() {
    serial(async {
        let dir = TempDir::new("fs-swap-write");
        let root = dir.file("ws");
        dir.write("ws/sub/sentinel.txt", "SENTINEL-CONTENT");
        let outside = sentinel_outside(&dir);
        let tools = tools(&root);

        swap_parent_once(&root, "sub", &outside);
        let result = call(
            &tools,
            "write",
            json!({"path": "sub/sentinel.txt", "content": "PWNED"}),
        )
        .await;
        clear_hook();
        assert_eq!(result.unwrap_err().code, "path_outside_workspace");
        assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");

        // Restore the directory for the patch round, then swap again.
        std::fs::remove_file(root.join("sub")).unwrap();
        dir.write("ws/sub/sentinel.txt", "SENTINEL-CONTENT");
        swap_parent_once(&root, "sub", &outside);
        let result = call(
            &tools,
            "patch",
            json!({"path": "sub/sentinel.txt", "old": "SENTINEL-CONTENT", "new": "PWNED"}),
        )
        .await;
        clear_hook();
        assert_eq!(result.unwrap_err().code, "path_outside_workspace");
        assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");
    });
}

#[test]
fn list_glob_and_grep_cannot_reach_outside_when_the_base_is_swapped_after_the_check() {
    serial(async {
        let dir = TempDir::new("fs-swap-search");
        let root = dir.file("ws");
        dir.write("ws/sub/a.txt", "SENTINEL inside");
        let outside = sentinel_outside(&dir);
        let tools = tools(&root);

        for (name, args) in [
            ("read", json!({"path": "sub"})),
            ("glob", json!({"pattern": "**/*", "path": "sub"})),
            ("grep", json!({"pattern": "SENTINEL", "path": "sub"})),
        ] {
            // Fresh directory each round, swapped by the hook mid-call.
            let _ = std::fs::remove_file(root.join("sub"));
            let _ = std::fs::remove_dir_all(root.join("sub"));
            dir.write("ws/sub/a.txt", "SENTINEL inside");
            swap_parent_once(&root, "sub", &outside);
            let result = call(&tools, name, args).await;
            clear_hook();
            let failure = result.expect_err("the swapped base must be refused");
            assert_eq!(
                failure.code, "path_outside_workspace",
                "{name}: {failure:?}"
            );
            assert!(!failure.message.contains("SENTINEL-CONTENT"), "{name}");
        }
    });
}

// ---------- a walk never descends into a link, and a `.gitignore` is still honoured ----------

#[test]
fn the_walk_skips_linked_directories_and_files_but_keeps_ignore_rules() {
    serial(async {
        let dir = TempDir::new("fs-walk");
        let root = dir.file("ws");
        dir.write("ws/keep/a.txt", "SENTINEL a");
        dir.write("ws/keep/.gitignore", "ignored.txt\n");
        dir.write("ws/keep/ignored.txt", "SENTINEL ignored");
        dir.write("ws/inner/b.txt", "SENTINEL b");
        let outside = sentinel_outside(&dir);
        std::os::unix::fs::symlink(&outside, root.join("keep/link_out")).unwrap();
        std::os::unix::fs::symlink(root.join("inner"), root.join("keep/link_in")).unwrap();
        std::os::unix::fs::symlink(root.join("inner/b.txt"), root.join("keep/link_file.txt"))
            .unwrap();
        let tools = tools(&root);

        let out = call(
            &tools,
            "glob",
            json!({"pattern": "**/*.txt", "path": "keep"}),
        )
        .await
        .unwrap();
        assert_eq!(text(&out), "keep/a.txt\n");

        let out = call(
            &tools,
            "grep",
            json!({"pattern": "SENTINEL", "path": "keep"}),
        )
        .await
        .unwrap();
        assert_eq!(text(&out), "keep/a.txt:1:SENTINEL a\n");
    });
}

#[test]
fn a_whole_tree_walk_from_the_root_honours_nested_ignore_files_and_git_exclude() {
    serial(async {
        let dir = TempDir::new("fs-walk-nested");
        let root = dir.file("ws");
        dir.write("ws/.gitignore", "build/\n");
        dir.write("ws/.git/info/exclude", "secret.txt\n");
        dir.write("ws/.git/HEAD", "ref: refs/heads/main\n");
        dir.write("ws/src/.ignore", "gen.rs\n");
        dir.write("ws/src/lib.rs", "x");
        dir.write("ws/src/gen.rs", "x");
        dir.write("ws/build/out.rs", "x");
        dir.write("ws/secret.txt", "x");
        dir.write("ws/kept.rs", "x");
        let tools = tools(&root);

        let out = call(&tools, "glob", json!({"pattern": "**/*"}))
            .await
            .unwrap();
        assert_eq!(text(&out), ".gitignore\nkept.rs\nsrc/.ignore\nsrc/lib.rs\n");
    });
}

// ---------- the fd is what gets used: a swap after the open changes nothing ----------

/// The counterpart of the swap tests: the parent is replaced **after** the tool has its fd.
/// The tool keeps reading and writing through that fd, so it still sees the inside file; an
/// implementation that re-opened the path at this point would follow the new link to the
/// outside sentinel instead.
#[test]
fn read_and_write_use_the_fd_they_opened_even_if_the_parent_is_swapped_afterwards() {
    serial(async {
        let dir = TempDir::new("fs-after-open");
        let root = dir.file("ws");
        dir.write("ws/sub/sentinel.txt", "INSIDE");
        let outside = sentinel_outside(&dir);
        let tools = tools(&root);

        let victim = root.join("sub");
        let target = outside.clone();
        let done = std::sync::atomic::AtomicBool::new(false);
        radian_coding::safe_fs::after_open_hook(Some(Box::new(move || {
            if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::fs::remove_dir_all(&victim).unwrap();
            std::os::unix::fs::symlink(&target, &victim).unwrap();
        })));
        let result = call(&tools, "read", json!({"path": "sub/sentinel.txt"})).await;
        clear_hook();
        let out = result.expect("the open fd is read; the later swap does not matter");
        assert_eq!(text(&out), "1: INSIDE");
        assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");

        // The same for write: the fd opened before the swap is the inside file.
        std::fs::remove_file(root.join("sub")).unwrap();
        dir.write("ws/sub/sentinel.txt", "INSIDE");
        let victim = root.join("sub");
        let target = outside.clone();
        let done = std::sync::atomic::AtomicBool::new(false);
        radian_coding::safe_fs::after_open_hook(Some(Box::new(move || {
            if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::fs::remove_dir_all(&victim).unwrap();
            std::os::unix::fs::symlink(&target, &victim).unwrap();
        })));
        let result = call(
            &tools,
            "write",
            json!({"path": "sub/sentinel.txt", "content": "WRITTEN"}),
        )
        .await;
        clear_hook();
        result.expect("the write goes to the fd opened before the swap");
        assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");
    });
}

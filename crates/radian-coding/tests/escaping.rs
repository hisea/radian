//! The workspace boundary, read/list half: absolute paths, `..`, symlink escapes,
//! symlinked parents, a symlinked workspace root, and the stated hard-link limit.

mod common;

use common::{TempDir, assert_outside, call, sentinel_outside, structured, text, tools};
use serde_json::json;

// ---------- 1. absolute path escape ----------

#[tokio::test]
async fn an_absolute_read_path_is_refused() {
    let dir = TempDir::new("abs-read");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "read", json!({"path": "/etc/passwd"}))
        .await
        .unwrap_err();
    assert_outside(&failure, "/etc/passwd");

    let failure = call(&tools, "list", json!({"path": "/etc"}))
        .await
        .unwrap_err();
    assert_outside(&failure, "/etc");
}

// ---------- 2. `..` escape ----------

#[tokio::test]
async fn parent_dir_escapes_are_refused() {
    let dir = TempDir::new("dotdot");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    let tools = tools(&root);

    for path in [
        "../outside/sentinel.txt",
        "a/../../outside/sentinel.txt",
        "..",
    ] {
        let failure = call(&tools, "read", json!({ "path": path }))
            .await
            .unwrap_err();
        assert_outside(&failure, path);
    }
    // `list a/../../..` is the same escape through `list`, which does exist.
    let failure = call(&tools, "list", json!({"path": "a/../../.."}))
        .await
        .unwrap_err();
    assert_outside(&failure, "a/../../..");
}

// ---------- 3. symlink escape (read) ----------

#[tokio::test]
async fn a_symlink_pointing_outside_is_refused_for_read() {
    let dir = TempDir::new("symlink-read");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    // A link inside the workspace to a file outside it.
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("leak.txt")).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "read", json!({"path": "leak.txt"}))
        .await
        .unwrap_err();
    assert_outside(&failure, "leak.txt");

    // A link to the outside directory, listed: the entry is shown but never followed.
    std::os::unix::fs::symlink(&outside, root.join("leak_dir")).unwrap();
    let listed = call(&tools, "list", json!({})).await.unwrap();
    let body = text(&listed);
    assert!(body.contains("symlink"), "{body}");
    assert!(!body.contains("SENTINEL-CONTENT"), "{body}");
}

// ---------- 4. symlinked parent directory escape ----------

#[tokio::test]
async fn a_symlinked_parent_directory_escape_is_refused() {
    let dir = TempDir::new("symlink-parent");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "read", json!({"path": "link_dir/sentinel.txt"}))
        .await
        .unwrap_err();
    assert_outside(&failure, "link_dir/sentinel.txt");

    let failure = call(&tools, "list", json!({"path": "link_dir"}))
        .await
        .unwrap_err();
    assert_outside(&failure, "link_dir");
}

// ---------- 8. the workspace root is itself a symlink ----------

#[tokio::test]
async fn a_symlinked_workspace_root_works_and_still_confines() {
    let dir = TempDir::new("root-symlink");
    let real = dir.file("real-ws");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("inside.txt"), "INSIDE").unwrap();
    let link = dir.file("ws-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // Constructing from the symlink canonicalizes it to the real directory.
    let tools = tools(&link);
    assert_eq!(tools.root(), real.canonicalize().unwrap());

    let output = call(&tools, "read", json!({"path": "inside.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&output), "INSIDE");

    let failure = call(&tools, "read", json!({"path": "/etc/passwd"}))
        .await
        .unwrap_err();
    assert_outside(&failure, "/etc/passwd");
}

// ---------- 6. hard links (read is a stated limitation) ----------

/// A pre-existing hard link inside the workspace to a file outside it has an in-workspace
/// canonical path, so `read` cannot tell where the other link lives. This test documents
/// the limitation as behaviour rather than pretending to block it.
#[tokio::test]
async fn a_hard_link_to_an_outside_file_is_readable_and_that_is_documented() {
    let dir = TempDir::new("hardlink-read");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::fs::hard_link(outside.join("sentinel.txt"), root.join("hard.txt")).unwrap();
    let tools = tools(&root);

    let output = call(&tools, "read", json!({"path": "hard.txt"}))
        .await
        .unwrap();
    assert_eq!(
        text(&output),
        "SENTINEL-CONTENT",
        "the crate documents that read cannot detect a hard link"
    );
}

// ---------- misc boundary shape ----------

#[tokio::test]
async fn a_missing_file_is_not_found_not_an_escape() {
    let dir = TempDir::new("missing");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "read", json!({"path": "nope.txt"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "not_found", "{failure:?}");
}

#[tokio::test]
async fn read_reports_truncation_and_the_total_size() {
    let dir = TempDir::new("truncate");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    // 10 ASCII bytes, read 4.
    std::fs::write(root.join("f.txt"), "0123456789").unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 4}))
        .await
        .unwrap();
    assert_eq!(text(&output), "0123");
    let info = structured(&output);
    assert_eq!(info["bytes_read"], 4);
    assert_eq!(info["total_bytes"], 10);
    assert_eq!(info["truncated"], true);
}

#[tokio::test]
async fn read_trims_an_incomplete_character_at_the_budget_edge() {
    let dir = TempDir::new("utf8-edge");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    // "é" is two bytes; a 1-byte budget cuts it in half.
    std::fs::write(root.join("f.txt"), "é").unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 1}))
        .await
        .unwrap();
    assert_eq!(text(&output), "", "the incomplete character is dropped");
}

#[tokio::test]
async fn read_refuses_a_binary_file() {
    let dir = TempDir::new("binary");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let failure = call(&tools, "read", json!({"path": "bin"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "not_utf8", "{failure:?}");
}

#[tokio::test]
async fn read_refuses_a_budget_above_the_hard_cap() {
    let dir = TempDir::new("cap");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "x").unwrap();

    let failure = call(
        &tools,
        "read",
        json!({"path": "f.txt", "max_bytes": 2_000_000}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "read_limit_exceeded", "{failure:?}");
}

#[tokio::test]
async fn list_reports_entries_sorted_with_kinds() {
    let dir = TempDir::new("list");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("b.txt"), "bb").unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    let tools = tools(&root);

    let output = call(&tools, "list", json!({})).await.unwrap();
    let body = text(&output);
    let names: Vec<&str> = body
        .lines()
        .map(|line| line.rsplit('\t').next().unwrap())
        .collect();
    assert_eq!(names, ["a.txt", "b.txt", "sub"]);
    assert!(
        body.lines()
            .any(|l| l.starts_with("dir\t") && l.ends_with("\tsub")),
        "{body}"
    );
    assert!(body.contains("file\t1\ta.txt"), "{body}");

    let info = structured(&output);
    assert_eq!(info["total_entries"], 3);
    assert_eq!(info["truncated"], false);
}

#[tokio::test]
async fn list_truncates_at_max_entries() {
    let dir = TempDir::new("list-cap");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..10 {
        std::fs::write(root.join(format!("f{i:02}")), "").unwrap();
    }
    let tools = tools(&root);

    let output = call(&tools, "list", json!({"max_entries": 3}))
        .await
        .unwrap();
    let info = structured(&output);
    assert_eq!(info["entries"], 3);
    assert_eq!(info["total_entries"], 10);
    assert_eq!(info["truncated"], true);
    assert_eq!(text(&output).lines().count(), 3);
}

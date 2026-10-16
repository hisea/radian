//! `write`/`patch`: their escape refusals, the hard-link rule, and patch's auditability.

mod common;

use common::{TempDir, assert_outside, call, sentinel_outside, structured, text, tools};
use serde_json::json;

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

// ---------- happy paths ----------

#[tokio::test]
async fn write_creates_and_overwrites_a_file_verbatim() {
    let dir = TempDir::new("write-happy");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let tools = tools(&root);

    let output = call(
        &tools,
        "write",
        json!({"path": "sub/a.txt", "content": "one\n"}),
    )
    .await
    .unwrap();
    assert_eq!(structured(&output)["bytes_written"], 4);
    // Verbatim: no trailing newline is added.
    assert_eq!(read(&root.join("sub/a.txt")), "one\n");

    call(
        &tools,
        "write",
        json!({"path": "sub/a.txt", "content": "two"}),
    )
    .await
    .unwrap();
    assert_eq!(read(&root.join("sub/a.txt")), "two");
}

/// ADR 0028: an overwrite's result carries the **old** file's line count
/// (`previous_lines`), so the TUI can show `+M −N`. The rule is `str::lines`': `\n`-separated,
/// a `\r\n` pair counts once, and a final line ending adds no line.
#[tokio::test]
async fn write_reports_the_overwritten_files_line_count() {
    let dir = TempDir::new("write-previous-lines");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    // A new file has no old content: the field is absent, not zero.
    let created = call(&tools, "write", json!({"path": "a.txt", "content": "x\n"}))
        .await
        .unwrap();
    assert!(
        structured(&created).get("previous_lines").is_none(),
        "a new file has no previous_lines: {:?}",
        structured(&created)
    );

    // An overwrite of a 3-line file (trailing newline included) reports 3.
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let overwrite = call(&tools, "write", json!({"path": "a.txt", "content": "x"}))
        .await
        .unwrap();
    assert_eq!(structured(&overwrite)["previous_lines"], 3);
    assert_eq!(structured(&overwrite)["bytes_written"], 1);

    // Without a trailing newline the count is the same: the final ending adds no line.
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree").unwrap();
    let no_trailing = call(&tools, "write", json!({"path": "a.txt", "content": "x"}))
        .await
        .unwrap();
    assert_eq!(structured(&no_trailing)["previous_lines"], 3);

    // A `\r\n` pair is one terminator, and an empty file counts zero (field absent).
    std::fs::write(root.join("a.txt"), "one\r\ntwo\r\n").unwrap();
    let crlf = call(&tools, "write", json!({"path": "a.txt", "content": "x"}))
        .await
        .unwrap();
    assert_eq!(structured(&crlf)["previous_lines"], 2);
    std::fs::write(root.join("a.txt"), "").unwrap();
    let empty = call(&tools, "write", json!({"path": "a.txt", "content": "x"}))
        .await
        .unwrap();
    assert!(
        structured(&empty).get("previous_lines").is_none(),
        "an empty old file has no previous_lines: {:?}",
        structured(&empty)
    );
}

/// The count streams the old file, so its size is independent of `max_write_bytes` (the cap
/// applies to the new `content` only). An old file far past a tiny cap still counts.
#[tokio::test]
async fn write_counts_an_old_file_larger_than_max_write_bytes() {
    let dir = TempDir::new("write-previous-lines-big");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools =
        radian_coding::CodingTools::new(&root)
            .unwrap()
            .with_limits(radian_coding::Limits {
                max_write_bytes: 4,
                ..Default::default()
            });

    let old: String = (0..1000).map(|i| format!("line {i}\n")).collect();
    assert!(old.len() > 4, "the old file is past the write cap");
    std::fs::write(root.join("big.txt"), &old).unwrap();

    let output = call(&tools, "write", json!({"path": "big.txt", "content": "x"}))
        .await
        .unwrap();
    assert_eq!(structured(&output)["previous_lines"], 1000);
    assert_eq!(read(&root.join("big.txt")), "x");
}

#[tokio::test]
async fn patch_replaces_exactly_one_occurrence() {
    let dir = TempDir::new("patch-happy");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "alpha beta gamma\n").unwrap();
    let tools = tools(&root);

    let output = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "beta", "new": "BETA"}),
    )
    .await
    .unwrap();
    assert_eq!(structured(&output)["replaced"], 1);
    assert_eq!(read(&root.join("f.txt")), "alpha BETA gamma\n");
}

// ---------- patch auditability: failures are visible and change nothing ----------

#[tokio::test]
async fn patch_refuses_a_missing_needle_and_leaves_the_file_alone() {
    let dir = TempDir::new("patch-missing");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "alpha\n").unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "beta", "new": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_not_found", "{failure:?}");
    assert_eq!(read(&root.join("f.txt")), "alpha\n");
}

#[tokio::test]
async fn patch_refuses_an_ambiguous_needle_and_leaves_the_file_alone() {
    let dir = TempDir::new("patch-ambiguous");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "x\nx\n").unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "x", "new": "y"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_ambiguous", "{failure:?}");
    assert!(failure.message.contains("2 times"), "{failure:?}");
    assert_eq!(
        read(&root.join("f.txt")),
        "x\nx\n",
        "no fuzzy match, no edit"
    );
}

// ---------- 3/5. writing through a symlink ----------

#[tokio::test]
async fn write_through_a_symlink_to_outside_is_refused() {
    let dir = TempDir::new("write-symlink-out");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("leak.txt")).unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "write",
        json!({"path": "leak.txt", "content": "pwned"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "leak.txt");
    assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");
}

#[tokio::test]
async fn write_refuses_a_symlink_even_inside_the_workspace() {
    let dir = TempDir::new("write-symlink-in");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("real.txt"), "real").unwrap();
    std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "write", json!({"path": "link.txt", "content": "x"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_is_symlink", "{failure:?}");
    assert_eq!(read(&root.join("real.txt")), "real");
}

#[tokio::test]
async fn write_refuses_a_dangling_symlink() {
    let dir = TempDir::new("write-dangling");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(root.join("does-not-exist"), root.join("dangling")).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "write", json!({"path": "dangling", "content": "x"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "path_is_symlink", "{failure:?}");
}

/// 5: a file is written, then the path is replaced by a link to outside, then written
/// again. The second write must refuse; the outside file must keep its content.
#[tokio::test]
async fn a_link_swapped_in_after_a_write_is_refused_by_the_next_write() {
    let dir = TempDir::new("write-link-swap");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    let tools = tools(&root);

    call(
        &tools,
        "write",
        json!({"path": "f.txt", "content": "first"}),
    )
    .await
    .unwrap();
    assert_eq!(read(&root.join("f.txt")), "first");

    // Something else replaces the file with a link to the outside.
    std::fs::remove_file(root.join("f.txt")).unwrap();
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("f.txt")).unwrap();

    let failure = call(
        &tools,
        "write",
        json!({"path": "f.txt", "content": "second"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "f.txt");
    assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");
}

// ---------- 4/9. symlinked parent ----------

#[tokio::test]
async fn write_into_an_outside_symlinked_directory_is_refused() {
    let dir = TempDir::new("write-parent-out");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
    let tools = tools(&root);

    // 9: the new file does not exist, so its parent is what must be checked.
    let failure = call(
        &tools,
        "write",
        json!({"path": "link_dir/new.txt", "content": "pwned"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "link_dir/new.txt");
    assert!(!outside.join("new.txt").exists());
}

#[tokio::test]
async fn write_to_a_missing_parent_directory_is_not_found() {
    let dir = TempDir::new("write-no-parent");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "write",
        json!({"path": "no/such/dir/f.txt", "content": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "not_found", "{failure:?}");
}

#[tokio::test]
async fn write_absolute_and_dotdot_paths_are_refused() {
    let dir = TempDir::new("write-paths");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    let tools = tools(&root);

    let failure = call(
        &tools,
        "write",
        json!({"path": outside.join("abs.txt").to_string_lossy(), "content": "x"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "absolute");

    let failure = call(
        &tools,
        "write",
        json!({"path": "../outside/dotdot.txt", "content": "x"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "../outside/dotdot.txt");
    assert!(!outside.join("abs.txt").exists());
    assert!(!outside.join("dotdot.txt").exists());
}

// ---------- 6. hard links ----------

#[tokio::test]
async fn write_and_patch_refuse_a_hard_linked_file() {
    let dir = TempDir::new("hardlink-write");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::fs::hard_link(outside.join("sentinel.txt"), root.join("hard.txt")).unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "write",
        json!({"path": "hard.txt", "content": "pwned"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "hard_link_refused", "{failure:?}");

    let failure = call(
        &tools,
        "patch",
        json!({"path": "hard.txt", "old": "SENTINEL", "new": "OWNED"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "hard_link_refused", "{failure:?}");

    assert_eq!(
        read(&outside.join("sentinel.txt")),
        "SENTINEL-CONTENT",
        "the outside file must be untouched"
    );
}

// ---------- patch escapes ----------

#[tokio::test]
async fn patch_through_an_outside_symlink_is_refused() {
    let dir = TempDir::new("patch-symlink");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("leak.txt")).unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "patch",
        json!({"path": "leak.txt", "old": "SENTINEL", "new": "OWNED"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "leak.txt");
    assert_eq!(read(&outside.join("sentinel.txt")), "SENTINEL-CONTENT");
}

#[tokio::test]
async fn patch_rejects_an_empty_needle() {
    let dir = TempDir::new("patch-empty");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "a").unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "", "new": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_invalid", "{failure:?}");
    assert_eq!(
        text(
            &call(&tools, "read", json!({"path": "f.txt"}))
                .await
                .unwrap()
        ),
        "1: a"
    );
}

//! ADR 0043 decision 1: `read` on a directory lists it — the old `list` output verbatim
//! (sorted `kind\tsize\tname` entries, symlinks shown but never followed, the same fd-level
//! open path) — with `limit` as the entry cap. `offset` and `max_bytes` are file arguments
//! and are ignored for a directory, said once when the caller passed them.

mod common;

use common::{TempDir, call, structured, text};
use radian_coding::{CodingTools, Limits, ReadRoots};
use serde_json::json;

fn tools_with_limits(root: &std::path::Path, limits: Limits) -> CodingTools {
    CodingTools::with_read_roots(root, ReadRoots::none())
        .unwrap()
        .with_limits(limits)
}

fn listing_limits() -> Limits {
    Limits {
        default_list_entries: 5,
        max_list_entries: 7,
        ..Limits::default()
    }
}

/// Item 10: the `limit` field says what it does for a directory (caps entries, with the
/// listing default and maximum), not only what it does for a file.
#[tokio::test]
async fn the_limit_field_names_the_directory_entry_cap() {
    use radian_core::{ToolProvider, ToolSession};
    let dir = TempDir::new("read-dir-desc");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let session = common::tools(&root).open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "read")
        .expect("the read spec");
    let field = &spec.input_schema["properties"]["limit"]["description"];
    assert!(
        field
            .as_str()
            .is_some_and(|text| text.contains("caps entries")),
        "the `limit` field must name the directory entry cap: {field}"
    );
}

/// One line of a listing, split on the tabs `read` separates with.
fn parse_line(line: &str) -> (&str, &str, &str) {
    let mut parts = line.splitn(3, '\t');
    (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    )
}

/// A directory `read` is the old `list` output verbatim: sorted entries with kind and size,
/// and the same `structured` shape.
#[tokio::test]
async fn read_on_a_directory_lists_it_like_list_did() {
    let dir = TempDir::new("read-dir");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("b.txt"), "bb").unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    let tools = common::tools(&root);

    let output = call(&tools, "read", json!({"path": "."})).await.unwrap();
    let body = text(&output);
    let lines: Vec<(&str, &str, &str)> = body.lines().map(parse_line).collect();
    assert_eq!(
        lines,
        [
            ("file", "1", "a.txt"),
            ("file", "2", "b.txt"),
            ("dir", lines[2].1, "sub"),
        ],
        "{body}"
    );

    let info = structured(&output);
    assert_eq!(info["path"], ".");
    assert_eq!(info["entries"], 3);
    assert_eq!(info["total_entries"], 3);
    assert_eq!(info["truncated"], false);
}

/// A symlink in the listed directory is shown, never followed.
#[tokio::test]
async fn read_on_a_directory_shows_symlinks_without_following_them() {
    let dir = TempDir::new("read-dir-symlink");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = common::sentinel_outside(&dir);
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("leak.txt")).unwrap();
    let tools = common::tools(&root);

    let output = call(&tools, "read", json!({"path": "."})).await.unwrap();
    let body = text(&output);
    assert!(
        body.lines().any(|line| {
            let (kind, _, name) = parse_line(line);
            kind == "symlink" && name == "leak.txt"
        }),
        "{body}"
    );
    assert!(!body.contains("SENTINEL-CONTENT"), "{body}");
}

/// `limit` caps the entries; past the default the cut is said in the body (the adapters never
/// send `structured` to the model, so the flag alone would be invisible).
#[tokio::test]
async fn limit_caps_directory_entries_and_the_cut_is_said() {
    let dir = TempDir::new("read-dir-limit");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..8 {
        std::fs::write(root.join(format!("f{i:02}")), "").unwrap();
    }
    let tools = tools_with_limits(&root, listing_limits());

    // An explicit `limit` caps the entries.
    let output = call(&tools, "read", json!({"path": ".", "limit": 3}))
        .await
        .unwrap();
    let body = text(&output);
    assert_eq!(body.lines().count(), 4, "{body}");
    assert!(
        body.ends_with("[truncated: showing 3 of 8 entries; narrow it with `glob` or `grep`]\n"),
        "{body}"
    );
    let info = structured(&output);
    assert_eq!(info["entries"], 3);
    assert_eq!(info["total_entries"], 8);
    assert_eq!(info["truncated"], true);

    // Without `limit` the listing default applies (5 here).
    let output = call(&tools, "read", json!({"path": "."})).await.unwrap();
    let info = structured(&output);
    assert_eq!(info["entries"], 5);
    assert_eq!(info["total_entries"], 8);
    assert_eq!(info["truncated"], true);

    // Past the hard cap the directory read is refused, like the old `list` was.
    let failure = call(&tools, "read", json!({"path": ".", "limit": 8}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "read_limit_exceeded", "{failure:?}");
}

/// `offset` and `max_bytes` are file arguments: for a directory they change nothing, and the
/// result says they were ignored — but only when the caller passed them, so the plain
/// listing stays byte-identical to the old `list` output.
#[tokio::test]
async fn offset_and_max_bytes_are_ignored_for_a_directory_and_said_once() {
    let dir = TempDir::new("read-dir-ignored");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    let tools = common::tools(&root);

    let plain = text(&call(&tools, "read", json!({"path": "."})).await.unwrap());
    assert!(!plain.contains("ignored"), "{plain}");

    for arguments in [
        json!({"path": ".", "offset": 2}),
        json!({"path": ".", "max_bytes": 1024}),
    ] {
        let output = call(&tools, "read", arguments).await.unwrap();
        let body = text(&output);
        assert!(
            body.starts_with(&plain),
            "the listing itself is unchanged: {body}"
        );
        assert!(
            body.contains("[offset and max_bytes are file arguments and were ignored"),
            "{body}"
        );
    }
}

/// The removed `list` name is an unknown tool; `read` lists directories.
#[tokio::test]
async fn the_removed_list_name_is_unknown() {
    let dir = TempDir::new("read-dir-alias");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    let tools = common::tools(&root);

    let failure = call(&tools, "list", json!({"path": "."}))
        .await
        .expect_err("the removed name is unknown");
    assert_eq!(failure.code, "unknown_tool", "{failure:?}");

    // `read` still lists the directory.
    let read = text(&call(&tools, "read", json!({"path": "."})).await.unwrap());
    assert!(read.contains("a.txt"), "{read}");
    assert!(read.contains("sub"), "{read}");
}

/// Files still read as files: the directory branch only fires on directories.
#[tokio::test]
async fn read_on_a_file_is_unchanged() {
    let dir = TempDir::new("read-dir-file");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
    let tools = common::tools(&root);

    let output = call(&tools, "read", json!({"path": "f.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&output), "1: alpha\n2: beta\n");
}

//! The bundled manual (`radian://docs/`): embedded pages, the virtual root,
//! refusals, and the size cap.

mod common;

use common::{TempDir, call, structured, text, tools};
use serde_json::json;

/// Every embedded page name, from the table itself.
fn page_names() -> Vec<&'static str> {
    radian_coding::docs::DOCS
        .iter()
        .map(|(name, _)| *name)
        .collect()
}

/// The docs root lists every page in directory shape.
#[tokio::test]
async fn the_docs_root_lists_every_page() {
    let dir = TempDir::new("docs-root");
    let tools = tools(dir.path());
    for root in ["radian://docs/", "radian://docs"] {
        let output = call(&tools, "read", json!({"path": root}))
            .await
            .expect("the docs root lists");
        let body = text(&output);
        for name in page_names() {
            assert!(body.contains(name), "{root} lists {name}:\n{body}");
        }
        let info = structured(&output);
        assert_eq!(
            info.get("total_entries").and_then(|v| v.as_u64()),
            Some(page_names().len() as u64),
            "{info:?}"
        );
    }
}

/// One page reads with the same offset/limit/max_bytes rules as a file.
#[tokio::test]
async fn a_docs_page_reads_like_a_file() {
    let dir = TempDir::new("docs-page");
    let tools = tools(dir.path());
    let first = page_names()[0];
    let path = format!("radian://docs/{first}");
    let full = call(&tools, "read", json!({"path": path}))
        .await
        .expect("a docs page reads");
    let full_text = text(&full);
    assert!(
        full_text.contains("1: "),
        "lines carry numbers:\n{full_text}"
    );
    let info = structured(&full);
    let total = info
        .get("total_bytes")
        .and_then(|v| v.as_u64())
        .expect("total_bytes");
    let embedded = radian_coding::docs::find(first).expect("the page exists");
    assert_eq!(total, embedded.len() as u64);

    // `offset` skips, `limit` caps, with the same continuation hint shape as a file.
    let part = call(
        &tools,
        "read",
        json!({"path": path, "offset": 2, "limit": 1}),
    )
    .await
    .expect("offset/limit works on a docs page");
    let part_text = text(&part);
    assert!(
        part_text.starts_with("2: "),
        "starts at line 2:\n{part_text}"
    );
    assert!(
        part_text.contains("[truncated: continue with offset="),
        "a capped read names the next offset:\n{part_text}"
    );

    // Stripped of line numbers, the body is the embedded text verbatim.
    {
        let mut stripped = String::new();
        for line in full_text.lines() {
            if let Some((_, body)) = line.split_once(": ") {
                stripped.push_str(body);
                stripped.push('\n');
            }
        }
        // The file may or may not end with a newline; compare without the final newline.
        let expected = embedded.strip_suffix('\n').unwrap_or(embedded);
        let got = stripped.strip_suffix('\n').unwrap_or(&stripped);
        assert_eq!(got, expected, "stripped read equals embedded text");
    }

    // `max_bytes` truncates the same way as a file.
    let capped = call(&tools, "read", json!({"path": path, "max_bytes": 16}))
        .await
        .expect("max_bytes works on a docs page");
    let capped_text = text(&capped);
    assert!(
        capped_text.contains("truncated at max_bytes") || capped_text.contains("[truncated:"),
        "a tiny budget truncates:\n{capped_text}"
    );
}

/// An unknown page names the available pages.
#[tokio::test]
async fn an_unknown_docs_page_lists_the_pages() {
    let dir = TempDir::new("docs-missing");
    let tools = tools(dir.path());
    let error = call(&tools, "read", json!({"path": "radian://docs/nope.md"}))
        .await
        .expect_err("an unknown page fails");
    assert_eq!(error.code, "not_found", "{error:?}");
    for name in page_names() {
        assert!(error.message.contains(name), "{name} listed: {error:?}");
    }
}

/// Search tools refuse the virtual root and point to `read`.
#[tokio::test]
async fn search_tools_refuse_the_virtual_root() {
    let dir = TempDir::new("docs-search");
    let tools = tools(dir.path());
    let page = format!("radian://docs/{}", page_names()[0]);
    for (tool, args) in [
        ("grep", json!({"pattern": "the", "path": page})),
        ("grep", json!({"pattern": "the", "path": "radian://docs/"})),
        ("glob", json!({"pattern": "**/*.md", "path": page})),
        (
            "glob",
            json!({"pattern": "**/*.md", "path": "radian://docs/"}),
        ),
    ] {
        let error = call(&tools, tool, args).await.expect_err("refused");
        assert!(
            error.message.contains("read radian://docs/"),
            "{tool} points to read: {error:?}"
        );
    }
}

/// Writers refuse any virtual path as read-only.
#[tokio::test]
async fn writers_refuse_virtual_paths() {
    let dir = TempDir::new("docs-write");
    let tools = tools(dir.path());
    let page = format!("radian://docs/{}", page_names()[0]);
    for (tool, args) in [
        ("write", json!({"path": page, "content": "x"})),
        ("write", json!({"path": "radian://docs/", "content": "x"})),
        ("write", json!({"path": "radian://other/x", "content": "x"})),
        ("patch", json!({"path": page, "old": "a", "new": "b"})),
        (
            "patch",
            json!({"path": "radian://docs/", "old": "a", "new": "b"}),
        ),
    ] {
        let error = call(&tools, tool, args).await.expect_err("refused");
        assert_eq!(error.code, "path_outside_workspace", "{tool}: {error:?}");
        assert!(error.message.contains("read-only"), "{tool}: {error:?}");
    }
}

/// The embedded manual stays small.
#[test]
fn the_embedded_manual_has_a_size_cap() {
    let total: usize = radian_coding::docs::DOCS
        .iter()
        .map(|(_, text)| text.len())
        .sum();
    assert!(
        total <= 512 * 1024,
        "embedded manual is {total} bytes, over the 512 KiB cap"
    );
    assert!(!radian_coding::docs::DOCS.is_empty(), "at least one page");
}

/// The user manual never mentions internal design numbers and never uses the word slice.
#[test]
fn the_user_manual_has_no_design_numbers_or_slices() {
    for (name, text) in radian_coding::docs::DOCS {
        assert!(!text.contains("ADR"), "{name} mentions ADR");
        assert!(!text.contains("slice"), "{name} mentions slice");
    }
}

/// Every built-in tool name appears in the manual.
#[test]
fn every_tool_name_appears_in_the_manual() {
    let all: String = radian_coding::docs::DOCS
        .iter()
        .map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join("\n");
    for tool in [
        "read",
        "write",
        "patch",
        "glob",
        "grep",
        "shell",
        "job",
        "memory_search",
        "ask_user",
        "skill",
        "subagent",
    ] {
        assert!(
            all.contains(tool),
            "tool `{tool}` is missing from docs/user/"
        );
    }
}

/// The hand-written table matches `docs/user/` on disk.
#[test]
fn the_table_matches_docs_user_on_disk() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("../../docs/user");
    let mut on_disk: Vec<(String, String)> = Vec::new();
    let entries = std::fs::read_dir(&dir).expect("docs/user exists");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).expect("page reads");
        on_disk.push((name, text));
    }
    on_disk.sort_by(|a, b| a.0.cmp(&b.0));
    let embedded: Vec<(String, String)> = radian_coding::docs::DOCS
        .iter()
        .map(|(n, t)| (n.to_string(), t.to_string()))
        .collect();
    assert_eq!(
        on_disk.len(),
        embedded.len(),
        "page count matches: disk {on_disk:?} vs table"
    );
    for ((disk_name, disk_text), (tab_name, tab_text)) in on_disk.iter().zip(embedded.iter()) {
        assert_eq!(disk_name, tab_name, "page name matches");
        assert_eq!(disk_text, tab_text, "page {disk_name} text matches");
    }
}

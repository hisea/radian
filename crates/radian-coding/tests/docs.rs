//! The bundled manual (`radian://docs/`): embedded pages, the virtual root,
//! refusals, and the size cap.

mod common;

use common::{TempDir, call, structured, text, tools};
use regex::Regex;
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

/// `index.md` is the manual's map: it names every other page as a list item (`` - `stem`: ``),
/// so a new page cannot ship without a line on the map.
#[test]
fn the_index_lists_every_page() {
    let index = radian_coding::docs::DOCS
        .iter()
        .find(|(name, _)| *name == "index.md")
        .map(|(_, text)| *text)
        .expect("index.md is in the table");
    for name in page_names() {
        let stem = name.trim_end_matches(".md");
        if stem == "index" {
            continue;
        }
        assert!(
            index.contains(&format!("- `{stem}`:")),
            "index.md does not list {name}"
        );
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

/// Plan wording that must never reach an ADR. The test reads this one list. An ordinary word such
/// as "time slices" does not match: a slice identifier needs a space and then a letter or digit.
const ADR_PLAN_WORDING: &[(&str, &str)] = &[
    ("a backlog path", r"docs/backlog"),
    ("a slice identifier", r"(?i)\bslice\s+[A-Za-z0-9]"),
    (
        "a named slice",
        r"(?i)\b(?:next|previous|this|last) slice\b",
    ),
    ("a named task", r"(?i)\b(?:next|later) task\b"),
];

/// An ADR is a snapshot of the decisions in force: it never links the backlog and never names a
/// slice or a next/later piece of work. The manual has the same rule for its own pages.
#[test]
fn the_adrs_are_snapshots_not_plans() {
    // The patterns must catch plan wording and leave ordinary prose alone, or the sweep below
    // would pass for the wrong reason.
    for (sample, expected) in [
        ("docs/backlog/goal-mode.md", true),
        ("(ADR 0045 slice A)", true),
        ("(bwrap, slice 0)", true),
        ("see the next slice", true),
        ("a later task", true),
        ("per-frame time slices remeasure", false),
        ("During time-sliced remeasure", false),
    ] {
        let hit = ADR_PLAN_WORDING.iter().any(|(what, pattern)| {
            Regex::new(pattern)
                .unwrap_or_else(|error| panic!("{what} is a valid pattern: {error}"))
                .is_match(sample)
        });
        assert_eq!(hit, expected, "patterns on {sample:?}");
    }

    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("../../docs/adr");
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("docs/adr exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "docs/adr holds ADRs");

    let patterns: Vec<(&str, Regex)> = ADR_PLAN_WORDING
        .iter()
        .map(|(what, pattern)| {
            let regex = Regex::new(pattern)
                .unwrap_or_else(|error| panic!("{what} is a valid pattern: {error}"));
            (*what, regex)
        })
        .collect();
    let mut hits: Vec<String> = Vec::new();
    for path in &paths {
        let name = path.file_name().expect("an ADR has a file name");
        let text = std::fs::read_to_string(path).expect("an ADR reads");
        for (index, line) in text.lines().enumerate() {
            for (what, pattern) in &patterns {
                if let Some(found) = pattern.find(line) {
                    hits.push(format!(
                        "docs/adr/{}:{}: {what} `{}`",
                        name.to_string_lossy(),
                        index + 1,
                        found.as_str()
                    ));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "an ADR states only the decisions in force:\n{}",
        hits.join("\n")
    );
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

/// The manual's image limits come from the code, not from prose that can drift: the byte limit,
/// the per-side limit and the provider-scaling threshold are the constants themselves.
#[test]
fn the_manual_names_the_image_limits_from_the_code() {
    let text = radian_coding::docs::find("tools.md").expect("tools.md is in the table");
    let mib = radian_coding::MAX_IMAGE_BYTES / (1024 * 1024);
    assert!(
        text.contains(&format!("{mib} MiB")),
        "tools.md names the {mib} MiB byte limit"
    );
    assert!(
        text.contains(&radian_coding::MAX_IMAGE_BYTES.to_string()),
        "tools.md names the exact byte limit"
    );
    assert!(
        text.contains(&radian_coding::MAX_IMAGE_SIDE.to_string()),
        "tools.md names the {} px side limit",
        radian_coding::MAX_IMAGE_SIDE
    );
    assert!(
        text.contains(&radian_coding::SCALE_NOTICE_SIDE.to_string()),
        "tools.md names the {} px scaling threshold",
        radian_coding::SCALE_NOTICE_SIDE
    );
    // And the two error codes the image branch can return.
    for code in ["image_too_large", "image_offset"] {
        assert!(text.contains(code), "tools.md names `{code}`");
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

/// The drift reader the manual checks share anchors a table to its heading: a decoy table in
/// an earlier section is never read, a table under a deeper `###` subheading still belongs to
/// its parent `##` heading, and a `|` line inside a fenced block is not a table.
#[test]
fn the_drift_reader_anchors_tables_to_their_heading() {
    let doc = "\
## Before

| Key | Value |
|---|---|
| `decoy` | a |

## Config

| Key | Value |
|---|---|
| `real` | b |

```text
| Key | Value |
|---|---|
| `fenced` | c |
```

### Nested

| Key | Value |
|---|---|
| `nested` | d |

## After

| Key | Value |
|---|---|
| `after` | e |
";
    let tables = radian_coding::drift::tables_under(doc, "test.md", "## Config");
    let rows: Vec<&Vec<String>> = tables.iter().flatten().collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].first().map(String::as_str), Some("`real`"));
    assert_eq!(rows[1].first().map(String::as_str), Some("`nested`"));
    for ignored in ["`decoy`", "`fenced`", "`after`"] {
        assert!(
            !rows
                .iter()
                .any(|row| row.first().is_some_and(|cell| cell == ignored)),
            "{ignored} is outside the anchor but was read: {rows:?}"
        );
    }
    let first = radian_coding::drift::table_under(doc, "test.md", "## Config");
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].first().map(String::as_str), Some("`real`"));
    assert_eq!(
        radian_coding::drift::split_cells("| `a` | `b\\|c` |"),
        vec!["`a`".to_string(), "`b|c`".to_string()],
        "an escaped `|` stays inside its cell"
    );
}

/// A missing heading fails with the page and the heading, so a renamed section cannot silently
/// stop checking.
#[test]
#[should_panic(expected = "test.md has no heading `## Gone`")]
fn the_drift_reader_names_a_missing_anchor() {
    radian_coding::drift::tables_under(
        "## Present\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        "test.md",
        "## Gone",
    );
}

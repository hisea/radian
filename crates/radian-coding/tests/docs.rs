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

/// The docs root lists every page in directory shape, the arc pages under `arc/`.
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
        assert!(
            body.contains("arc/tui.md"),
            "{root} lists an arc page under its directory:\n{body}"
        );
        let info = structured(&output);
        assert_eq!(
            info.get("total_entries").and_then(|v| v.as_u64()),
            Some(page_names().len() as u64),
            "{info:?}"
        );
    }
}

/// A bundled-manual directory opens its `index.md`; a page under it reads by its path.
#[tokio::test]
async fn a_docs_directory_opens_its_index() {
    let dir = TempDir::new("docs-dir");
    let tools = tools(dir.path());
    let page = call(&tools, "read", json!({"path": "radian://docs/arc/mcp.md"}))
        .await
        .expect("a nested docs page reads");
    let page_text = text(&page);
    let embedded = radian_coding::docs::find("arc/mcp.md").expect("arc/mcp.md exists");
    assert!(
        embedded
            .lines()
            .next()
            .is_some_and(|line| page_text.contains(line)),
        "the nested page's first line is in the read:\n{page_text}"
    );
    let index = radian_coding::docs::find("arc/index.md").expect("arc/index.md exists");
    for path in ["radian://docs/arc/", "radian://docs/arc"] {
        let output = call(&tools, "read", json!({"path": path}))
            .await
            .expect("a docs directory opens its index");
        let body = text(&output);
        assert!(
            body.contains("# arc"),
            "{path} returns arc/index.md:\n{body}"
        );
        assert!(
            index
                .lines()
                .any(|line| line.starts_with("# arc") && body.contains(line)),
            "{path} returns the index page"
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

/// Defaults, case 8: a manual page keeps its own 64 KiB budget and has no line default, so a
/// page over both file defaults still reads whole with no note. `configuration.md` is over
/// 16 KiB and over 200 lines; `tools.md` is the page the manual names for `read`.
#[tokio::test]
async fn a_docs_page_reads_whole_beyond_the_file_defaults() {
    let dir = TempDir::new("docs-page-defaults");
    let tools = tools(dir.path());
    let limits = radian_coding::Limits::default();
    for name in ["tools.md", "configuration.md"] {
        let embedded = radian_coding::docs::find(name).expect("the page exists");
        let output = call(
            &tools,
            "read",
            json!({"path": format!("radian://docs/{name}")}),
        )
        .await
        .expect("a docs page reads");
        let info = structured(&output);
        assert_eq!(
            info["bytes_read"].as_u64(),
            Some(embedded.len() as u64),
            "{name} reads whole: {info}"
        );
        assert_eq!(
            info["end_line"].as_u64(),
            Some(embedded.lines().count() as u64),
            "{name} reads every line: {info}"
        );
        assert_eq!(info["truncated"], false, "{name}: {info}");
        assert!(info.get("next_offset").is_none(), "{name}: {info}");
        // The page's own text may discuss truncation notes; the read must not append one, so
        // the body ends with the page's last line instead of a tool note.
        let last_line = embedded.lines().last().unwrap_or_default();
        let last_numbered = format!("{}: {}\n", embedded.lines().count(), last_line);
        assert!(
            text(&output).ends_with(&last_numbered),
            "{name} ends with its own last line, not a note:\n{}",
            text(&output)
        );
        if name == "configuration.md" {
            assert!(
                embedded.len() > limits.default_read_bytes,
                "the page is over the file byte default"
            );
            assert!(
                embedded.lines().count() > limits.default_read_lines,
                "the page is over the file line default"
            );
        }
    }
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

/// Every page stays under 56 KiB (57,344 bytes), below the manual's own 65,536-byte read
/// budget (`Limits::default_manual_read_bytes`): a page that drifts up would no longer read
/// whole, which is what `a_docs_page_reads_like_a_file` and `radian docs <topic>` rely on. The
/// failure names the page and its size.
#[test]
fn every_user_manual_page_is_at_most_56_kib() {
    const CAP: usize = 56 * 1024;
    for (name, text) in radian_coding::docs::DOCS {
        assert!(
            text.len() <= CAP,
            "docs/user/{name} is {} bytes, over the {CAP}-byte page cap",
            text.len()
        );
    }
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
    // And the error code the image branch can return.
    assert!(
        text.contains("image_too_large"),
        "tools.md names `image_too_large`"
    );
    // A range is ignored for an image, and the manual says so instead of naming an error.
    assert!(
        text.contains("offset and limit ignored: an image is read whole"),
        "tools.md names the ignore line"
    );
    // The `limit` minimum is conditional: it applies to text and directory reads, while an
    // image ignores the value, so the manual never promises it unconditionally.
    assert!(
        text.contains("at least 1 for text and directory reads"),
        "tools.md makes the `limit` minimum conditional"
    );
    assert!(
        !text.contains("image_offset"),
        "tools.md drops `image_offset`"
    );
}

/// `grep`'s caps are the code's, not prose that can drift: the manual's `## \`grep\`` table
/// and prose name the default total match cap, the per-file cap, the context cut and the
/// byte cap, all read from `Limits`.
#[test]
fn the_manual_names_the_grep_limits_from_the_code() {
    let text = radian_coding::docs::find("tools.md").expect("tools.md is in the table");
    let limits = radian_coding::Limits::default();
    let rows = radian_coding::drift::table_under(text, "tools.md", "## `grep`");
    let row = |name: &str| {
        rows.iter()
            .find(|row| {
                row.first()
                    .is_some_and(|cell| cell.trim_matches('`') == name)
            })
            .unwrap_or_else(|| panic!("tools.md's `grep` table has a `{name}` row: {rows:?}"))
    };
    let default_matches = limits.default_grep_matches.to_string();
    assert_eq!(
        row("max_matches").get(2).map(|cell| cell.trim_matches('`')),
        Some(default_matches.as_str()),
        "the `max_matches` default comes from the code: {:?}",
        row("max_matches")
    );
    let context = row("context").join(" ");
    assert!(
        context.contains(&format!(
            "Values above {} are cut to {}",
            limits.max_grep_context, limits.max_grep_context
        )),
        "the `context` cut comes from the code: {context}"
    );
    assert!(
        text.contains(&format!(
            "per-file cap is {} matches",
            limits.default_grep_matches_per_file
        )),
        "tools.md names the {} per-file cap",
        limits.default_grep_matches_per_file
    );
    assert!(
        text.contains(&format!("capped at {} bytes", limits.max_grep_output_bytes)),
        "tools.md names the {} byte cap",
        limits.max_grep_output_bytes
    );
    assert!(
        text.contains(&format!("larger than {} bytes", limits.max_grep_file_bytes)),
        "tools.md names the {} file-size cap",
        limits.max_grep_file_bytes
    );
    assert!(
        text.contains(&format!(
            "[skipped <n> files over {} bytes",
            limits.max_grep_file_bytes
        )),
        "tools.md shows the note's shape with the {} file-size cap",
        limits.max_grep_file_bytes
    );
}

/// `read`'s limits are the code's, not prose that can drift: the manual's `## \`read\`` table
/// shows the byte budget, the file line cap and both directory entry counts from `Limits`.
#[test]
fn the_manual_names_the_read_limits_from_the_code() {
    let text = radian_coding::docs::find("tools.md").expect("tools.md is in the table");
    let limits = radian_coding::Limits::default();
    let rows = radian_coding::drift::table_under(text, "tools.md", "## `read`");
    let row = |name: &str| {
        rows.iter()
            .find(|row| {
                row.first()
                    .is_some_and(|cell| cell.trim_matches('`') == name)
            })
            .unwrap_or_else(|| panic!("tools.md's `read` table has a `{name}` row: {rows:?}"))
    };
    let default_bytes = limits.default_read_bytes.to_string();
    assert_eq!(
        row("max_bytes").get(2).map(|cell| cell.trim_matches('`')),
        Some(default_bytes.as_str()),
        "the `max_bytes` default comes from the code: {:?}",
        row("max_bytes")
    );
    let limit = row("limit");
    let default_cell = limit.get(2).map(String::as_str).unwrap_or_default();
    let default_lines = limits.default_read_lines.to_string();
    let default_entries = limits.default_list_entries.to_string();
    assert!(
        default_cell.contains(&default_lines),
        "the `limit` default names {} lines for a file: {default_cell}",
        limits.default_read_lines
    );
    assert!(
        default_cell.contains(&default_entries),
        "the `limit` default names {} entries for a directory: {default_cell}",
        limits.default_list_entries
    );
    let max_entries = limits.max_list_entries.to_string();
    assert!(
        limit.iter().any(|cell| cell.contains(&max_entries)),
        "the `limit` row names the {} entry maximum: {limit:?}",
        limits.max_list_entries
    );
}

/// The hand-written table matches `docs/user/` on disk, including the pages in subdirectories.
#[test]
fn the_table_matches_docs_user_on_disk() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("../../docs/user");
    let mut on_disk: Vec<(String, String)> = Vec::new();
    collect_pages(&dir, &dir, &mut on_disk);
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

/// Every relative link in a manual page names a file that exists, resolved from the page
/// that carries it: `themes.md` means `docs/user/arc/themes.md` on an arc page and
/// `docs/user/themes.md` on a top-level one, so an arc page reaches the theme page as
/// `../themes.md`. A target with no `.md` is not a page link: the manual shows the footer's
/// inline-format syntax `[text](style)`.
#[test]
fn every_relative_link_in_the_manual_resolves() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("../../docs/user");
    let mut pages: Vec<(String, String)> = Vec::new();
    collect_pages(&dir, &dir, &mut pages);
    let mut broken: Vec<String> = Vec::new();
    for (name, text) in &pages {
        let from = std::path::Path::new(name)
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        for (index, line) in text.lines().enumerate() {
            for target in link_targets(line) {
                let path = target.split('#').next().unwrap_or_default();
                if path.is_empty() || path.contains("://") || !path.ends_with(".md") {
                    continue;
                }
                if !dir.join(&from).join(path).is_file() {
                    broken.push(format!(
                        "docs/user/{name}:{} links `{target}`, which is not docs/user/{from}/{path}",
                        index + 1,
                        from = from.display()
                    ));
                }
            }
        }
    }
    assert!(
        broken.is_empty(),
        "manual links do not resolve:\n{}",
        broken.join("\n")
    );
}

/// Every `](...)` target on one line of Markdown, in order.
fn link_targets(line: &str) -> Vec<&str> {
    let mut targets = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        let end = after.find(')').unwrap_or(after.len());
        targets.push(&after[..end]);
        rest = &after[end..];
    }
    targets
}

/// Every `*.md` page under `dir`, named by its path relative to `base` with `/` separators.
fn collect_pages(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{} reads: {error}", dir.display()))
        .flatten()
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_pages(&path, base, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .strip_prefix(base)
            .expect("a page is under docs/user")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&path).expect("page reads");
        out.push((name, text));
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

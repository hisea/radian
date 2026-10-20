//! The two search tools (`glob`, `grep`): matching and non-matching, the ignore rules, the
//! workspace boundary, the caps, and the two spec facts (present, `Shared`).

mod common;

use agent_core::{ToolConcurrency, ToolProvider, ToolSession};
use common::{TempDir, assert_outside, call, sentinel_outside, structured, text, tools};
use radian_coding::{CodingTools, Limits};
use serde_json::json;

/// The tools with non-default limits, for the cap tests.
fn tools_with(root: &std::path::Path, limits: Limits) -> CodingTools {
    CodingTools::new(root).unwrap().with_limits(limits)
}

// ---------- glob: matching ----------

#[tokio::test]
async fn glob_matches_by_path_and_skips_non_matches() {
    let dir = TempDir::new("glob-basic");
    let root = dir.file("ws");
    dir.write("ws/src/a.rs", "fn a() {}");
    dir.write("ws/src/nested/b.rs", "fn b() {}");
    dir.write("ws/README.md", "# hi");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "**/*.rs"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "src/a.rs\nsrc/nested/b.rs\n");
    assert_eq!(structured(&out)["matches"], json!(2));
    assert_eq!(structured(&out)["total_matches"], json!(2));
    assert_eq!(structured(&out)["truncated"], json!(false));

    // A pattern that matches nothing says so instead of returning empty text.
    let out = call(&tools, "glob", json!({"pattern": "**/*.ts"}))
        .await
        .unwrap();
    assert!(text(&out).contains("no files matched"), "{}", text(&out));
}

/// `**` crosses directories and also matches at the top: `src/**/*.rs` finds `src/a.rs` as well
/// as `src/nested/b.rs` (the tool description's own example).
#[tokio::test]
async fn a_double_star_matches_zero_or_more_directories() {
    let dir = TempDir::new("glob-doublestar");
    let root = dir.file("ws");
    dir.write("ws/src/a.rs", "fn a() {}");
    dir.write("ws/src/nested/b.rs", "fn b() {}");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "src/**/*.rs"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "src/a.rs\nsrc/nested/b.rs\n");
}

/// A single `*` does not cross a `/`: `src/*.rs` finds only the immediate files.
#[tokio::test]
async fn a_single_star_stays_within_one_directory() {
    let dir = TempDir::new("glob-singlestar");
    let root = dir.file("ws");
    dir.write("ws/src/a.rs", "fn a() {}");
    dir.write("ws/src/nested/b.rs", "fn b() {}");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "src/*.rs"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "src/a.rs\n");
}

#[tokio::test]
async fn glob_scopes_to_the_path_argument() {
    let dir = TempDir::new("glob-path");
    let root = dir.file("ws");
    dir.write("ws/src/a.rs", "fn a() {}");
    dir.write("ws/other/b.rs", "fn b() {}");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "**/*.rs", "path": "src"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "src/a.rs\n");
}

// ---------- the ignore rules and `.git` ----------

#[tokio::test]
async fn the_ignore_rules_are_respected_by_both_tools() {
    let dir = TempDir::new("search-ignore");
    let root = dir.file("ws");
    dir.write("ws/.gitignore", "ignored.txt\nbuild/\n");
    dir.write("ws/kept.txt", "needle here");
    dir.write("ws/ignored.txt", "needle here");
    dir.write("ws/build/generated.txt", "needle here");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "**/*.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "kept.txt\n");

    let out = call(&tools, "grep", json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "kept.txt:1:needle here\n");
}

#[tokio::test]
async fn the_dot_git_directory_is_skipped_by_both_tools() {
    let dir = TempDir::new("search-dotgit");
    let root = dir.file("ws");
    dir.write("ws/.git/config", "needle");
    dir.write("ws/kept.txt", "needle");
    let tools = tools(&root);

    // `**/config` matches `.git/config` if and only if the directory was walked.
    let out = call(&tools, "glob", json!({"pattern": "**/config"}))
        .await
        .unwrap();
    assert!(!text(&out).contains(".git"), "{}", text(&out));

    let out = call(&tools, "grep", json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "kept.txt:1:needle\n");
}

/// `.radian-worktrees/` (one full checkout per worktree) is pruned **structurally**, not by
/// a `.gitignore` coincidence: even with no ignore file, the walk never descends into it.
#[tokio::test]
async fn the_radian_worktrees_directory_is_skipped_by_both_tools() {
    let dir = TempDir::new("search-worktrees");
    let root = dir.file("ws");
    // No `.gitignore` at all: the skip must not depend on one.
    dir.write("ws/.radian-worktrees/t1/leak.txt", "needle");
    dir.write("ws/kept.txt", "needle");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "**/*.txt"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "kept.txt\n");

    let out = call(&tools, "grep", json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "kept.txt:1:needle\n");
}

/// A symlink out of the workspace is never followed: the linked file is not a match and the
/// linked directory is not descended, so its contents are never read.
#[tokio::test]
async fn a_symlink_out_of_the_workspace_is_never_followed() {
    let dir = TempDir::new("search-symlink");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
    std::os::unix::fs::symlink(outside.join("sentinel.txt"), root.join("leak.txt")).unwrap();
    dir.write("ws/real.txt", "inside");
    let tools = tools(&root);

    let out = call(&tools, "glob", json!({"pattern": "**/*"}))
        .await
        .unwrap();
    let body = text(&out);
    // The positive control: the walk works and finds the real file.
    assert!(body.contains("real.txt"), "{body}");
    assert!(!body.contains("leak.txt"), "{body}");
    assert!(!body.contains("link_dir"), "{body}");

    // The sentinel text lives only through the link; grep must not read it.
    let out = call(&tools, "grep", json!({"pattern": "SENTINEL-CONTENT"}))
        .await
        .unwrap();
    let body = text(&out);
    // No match line: the only output is the "no matches" note naming the pattern.
    assert_eq!(body, "[no matches for `SENTINEL-CONTENT`]\n", "{body}");
    assert!(!body.contains("sentinel.txt"), "{body}");
}

// ---------- the workspace boundary ----------

#[tokio::test]
async fn the_boundary_refuses_a_path_outside_the_workspace() {
    let dir = TempDir::new("search-boundary");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let _ = sentinel_outside(&dir);
    let tools = tools(&root);

    for path in ["../outside", "..", "/tmp", "a/../../outside"] {
        let failure = call(&tools, "glob", json!({"pattern": "**/*", "path": path}))
            .await
            .unwrap_err();
        assert_outside(&failure, path);

        let failure = call(&tools, "grep", json!({"pattern": "x", "path": path}))
            .await
            .unwrap_err();
        assert_outside(&failure, path);
    }
}

/// A path that names a symlink pointing outside is refused by the same resolution the file
/// tools use (the base is canonicalized before the containment check).
#[tokio::test]
async fn a_path_through_a_symlink_outside_is_refused() {
    let dir = TempDir::new("search-path-symlink");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "glob",
        json!({"pattern": "**/*", "path": "link_dir"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "link_dir");
}

// ---------- caps ----------

#[tokio::test]
async fn glob_caps_the_result_and_names_the_cut() {
    let dir = TempDir::new("glob-cap");
    let root = dir.file("ws");
    for i in 0..5 {
        dir.write(&format!("ws/f{i}.txt"), "x");
    }
    let limits = Limits {
        default_glob_results: 2,
        ..Limits::default()
    };
    let tools = tools_with(&root, limits);

    let out = call(&tools, "glob", json!({"pattern": "*.txt"}))
        .await
        .unwrap();
    assert_eq!(
        text(&out),
        "f0.txt\nf1.txt\n[truncated: showing 2 of 5 matches]\n"
    );
    assert_eq!(structured(&out)["truncated"], json!(true));
    assert_eq!(structured(&out)["total_matches"], json!(5));

    // The model can lower the cap, and one above the hard limit is a named error.
    let out = call(
        &tools,
        "glob",
        json!({"pattern": "*.txt", "max_results": 1}),
    )
    .await
    .unwrap();
    assert_eq!(text(&out), "f0.txt\n[truncated: showing 1 of 5 matches]\n");
    let failure = call(
        &tools,
        "glob",
        json!({"pattern": "*.txt", "max_results": 9000}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "glob_limit_exceeded", "{failure:?}");
}

#[tokio::test]
async fn grep_caps_the_matches_per_file_and_names_the_cut() {
    let dir = TempDir::new("grep-cap-file");
    let root = dir.file("ws");
    dir.write("ws/f.txt", "hit one\nhit two\nhit three\n");
    let limits = Limits {
        default_grep_matches_per_file: 2,
        ..Limits::default()
    };
    let tools = tools_with(&root, limits);

    let out = call(&tools, "grep", json!({"pattern": "hit"}))
        .await
        .unwrap();
    let body = text(&out);
    assert_eq!(
        body,
        "f.txt:1:hit one\nf.txt:2:hit two\n\
         [f.txt: more than 2 matches in one file]\n\
         [truncated: showing the first 2 matches]\n",
        "{body}"
    );
    assert_eq!(structured(&out)["truncated"], json!(true));
}

#[tokio::test]
async fn grep_caps_the_total_matches_and_names_the_cut() {
    let dir = TempDir::new("grep-cap-total");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "hit\nhit\n");
    dir.write("ws/b.txt", "hit\nhit\n");
    dir.write("ws/c.txt", "hit\nhit\n");
    let tools = tools(&root);

    let out = call(&tools, "grep", json!({"pattern": "hit", "max_matches": 3}))
        .await
        .unwrap();
    let body = text(&out);
    assert_eq!(
        body,
        "a.txt:1:hit\na.txt:2:hit\nb.txt:1:hit\n\
         [truncated: showing the first 3 matches]\n",
        "{body}"
    );
    assert_eq!(structured(&out)["truncated"], json!(true));

    let failure = call(
        &tools,
        "grep",
        json!({"pattern": "hit", "max_matches": 9000}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "grep_limit_exceeded", "{failure:?}");
}

#[tokio::test]
async fn grep_caps_the_context_argument() {
    let dir = TempDir::new("grep-cap-context");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "hit\n");
    let tools = tools(&root);

    // Above the limit is cut, not refused (a refused call cost the model a round trip).
    let out = call(&tools, "grep", json!({"pattern": "hit", "context": 99}))
        .await
        .unwrap();
    assert!(text(&out).contains("a.txt:1:hit"), "{}", text(&out));
    assert!(
        text(&out).contains("[context cut to 30 lines]"),
        "{}",
        text(&out)
    );
    assert_eq!(out.structured.as_ref().unwrap()["context"], 30);
    // A request within the limit says nothing about a cut.
    let within = call(&tools, "grep", json!({"pattern": "hit", "context": 30}))
        .await
        .unwrap();
    assert!(!text(&within).contains("context cut"), "{}", text(&within));
}

/// The schema the model validates against must not refuse a large `context` before the tool
/// can cut it (agent-core validates arguments against the spec before the call).
#[tokio::test]
async fn grep_schema_does_not_cap_context_below_the_cut() {
    let dir = TempDir::new("grep-schema");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "x\n");
    let session = tools(&root).open_session().await.unwrap();
    let schema = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "grep")
        .expect("grep spec")
        .input_schema
        .clone();
    assert!(
        schema["properties"]["context"].get("maximum").is_none(),
        "{schema}"
    );
}

// ---------- grep: output, options, binary and regex errors ----------

#[tokio::test]
async fn grep_reports_matches_with_path_line_and_text() {
    let dir = TempDir::new("grep-basic");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "alpha\nbeta\nAlpha gamma\n");
    dir.write("ws/b.txt", "nothing\n");
    let tools = tools(&root);

    let out = call(&tools, "grep", json!({"pattern": "alpha"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "a.txt:1:alpha\n");
    assert_eq!(structured(&out)["matches"], json!(1));
    assert_eq!(structured(&out)["files"], json!(1));

    let out = call(
        &tools,
        "grep",
        json!({"pattern": "alpha", "case_insensitive": true}),
    )
    .await
    .unwrap();
    assert_eq!(text(&out), "a.txt:1:alpha\na.txt:3:Alpha gamma\n");

    let out = call(&tools, "grep", json!({"pattern": "absent"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "[no matches for `absent`]\n");
}

#[tokio::test]
async fn grep_shows_context_lines_and_separates_groups() {
    let dir = TempDir::new("grep-context");
    let root = dir.file("ws");
    dir.write("ws/c.txt", "l1\nl2\nmatch\nl4\nl5\nl6\nl7\nl8\nmatch2\n");
    let tools = tools(&root);

    let out = call(&tools, "grep", json!({"pattern": "match", "context": 1}))
        .await
        .unwrap();
    assert_eq!(
        text(&out),
        "c.txt-2-l2\nc.txt:3:match\nc.txt-4-l4\n--\nc.txt-8-l8\nc.txt:9:match2\n"
    );
}

#[tokio::test]
async fn grep_skips_binary_files() {
    let dir = TempDir::new("grep-binary");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("bin.dat"), b"needle\x00more").unwrap();
    dir.write("ws/txt.txt", "needle here");
    let tools = tools(&root);

    let out = call(&tools, "grep", json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "txt.txt:1:needle here\n");
}

#[tokio::test]
async fn grep_filters_files_by_glob() {
    let dir = TempDir::new("grep-glob-filter");
    let root = dir.file("ws");
    dir.write("ws/a.rs", "needle");
    dir.write("ws/b.txt", "needle");
    let tools = tools(&root);

    let out = call(&tools, "grep", json!({"pattern": "needle", "glob": "*.rs"}))
        .await
        .unwrap();
    assert_eq!(text(&out), "a.rs:1:needle\n");
}

#[tokio::test]
async fn an_invalid_regex_is_a_named_tool_error() {
    let dir = TempDir::new("grep-bad-regex");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "grep", json!({"pattern": "a("}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "invalid_regex", "{failure:?}");
    assert!(failure.message.contains("a("), "{failure:?}");
}

#[tokio::test]
async fn an_invalid_glob_is_a_named_tool_error() {
    let dir = TempDir::new("glob-bad-pattern");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "glob", json!({"pattern": "a["}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "invalid_glob", "{failure:?}");
}

// ---------- the specs ----------

#[tokio::test]
async fn both_search_tools_are_advertised_and_shared() {
    let dir = TempDir::new("search-specs");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let session = tools(&root).open_session().await.unwrap();

    for name in ["glob", "grep"] {
        let spec = session
            .tool_specs()
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("the {name} spec"));
        assert_eq!(
            spec.concurrency,
            ToolConcurrency::Shared,
            "{name} must be Shared so a batch of searches overlaps"
        );
    }
}

// ---------- the total byte cap and the output modes ----------

/// Many long matching lines: the output stays within the byte cap, ends with the one
/// byte-limit note, and keeps whole lines only (every line ends at a newline and every
/// shown match line is byte-identical to a file line).
#[tokio::test]
async fn grep_byte_cap_bounds_long_lines_with_whole_lines_only() {
    let dir = TempDir::new("grep-byte-cap");
    let root = dir.file("ws");
    // Twenty 500-byte matching lines: ~10 KiB of matches against a 2 KiB cap.
    let line = format!("needle-{}", "x".repeat(490));
    let content = (0..20).map(|_| line.clone()).collect::<Vec<_>>().join("\n") + "\n";
    dir.write("ws/f.txt", &content);
    let tools = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 2048,
            ..Limits::default()
        },
    );

    let out = call(&tools, "grep", json!({"pattern": "needle"}))
        .await
        .unwrap();
    let body = text(&out);
    assert!(
        body.len() <= 2048,
        "output {}B passes the 2048B cap",
        body.len()
    );
    assert!(
        body.contains("2048 byte limit"),
        "the one note names the byte limit: {body}"
    );
    assert!(
        body.contains("output: \"files\""),
        "the note says how to narrow it: {body}"
    );
    assert!(
        body.ends_with("]\n"),
        "the output ends with the note: {body}"
    );
    // Whole lines only: every shown match line is exactly one file line with its prefix.
    for output_line in body.lines() {
        if output_line.starts_with('[') || output_line == "--" {
            continue; // the truncation notes and the group separator
        }
        let rest = output_line.strip_prefix("f.txt:").unwrap_or_else(|| {
            panic!("a match line carries the `path:line:` prefix: {output_line}")
        });
        let (_, text_part) = rest
            .split_once(':')
            .unwrap_or_else(|| panic!("a match line carries `line:text`: {output_line}"));
        assert_eq!(
            text_part, line,
            "a shown line is a whole file line, never a prefix: {output_line}"
        );
    }
    assert_eq!(structured(&out)["truncated"], json!(true));
}

/// `output: "files"` returns one line per matching file with its match count, and no line
/// text at all.
#[tokio::test]
async fn grep_files_mode_returns_paths_and_counts_without_line_text() {
    let dir = TempDir::new("grep-files-mode");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "needle one\nneedle two\nother\n");
    dir.write("ws/b.txt", "needle three\nother\n");
    dir.write("ws/c.txt", "nothing here\n");
    let tools = tools(&root);

    let out = call(
        &tools,
        "grep",
        json!({"pattern": "needle", "output": "files"}),
    )
    .await
    .unwrap();
    let body = text(&out);
    assert!(body.contains("a.txt"), "{body}");
    assert!(body.contains('2'), "a.txt's count is shown: {body}");
    assert!(body.contains("b.txt"), "{body}");
    assert!(
        !body.contains("c.txt"),
        "a file without matches is absent: {body}"
    );
    assert!(
        !body.contains("needle"),
        "no line text is returned, only paths and counts: {body}"
    );
    assert_eq!(structured(&out)["matches"], json!(3));
    assert_eq!(structured(&out)["files"], json!(2));
    assert_eq!(structured(&out)["output"], json!("files"));
}

/// `output: "count"` returns only the totals: no paths, no line text.
#[tokio::test]
async fn grep_count_mode_returns_totals_only() {
    let dir = TempDir::new("grep-count-mode");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "needle one\nneedle two\n");
    dir.write("ws/b.txt", "needle three\n");
    let tools = tools(&root);

    let out = call(
        &tools,
        "grep",
        json!({"pattern": "needle", "output": "count"}),
    )
    .await
    .unwrap();
    let body = text(&out);
    assert!(body.contains('3'), "the total matches are shown: {body}");
    assert!(body.contains('2'), "the file count is shown: {body}");
    assert!(
        !body.contains("a.txt") && !body.contains("b.txt"),
        "no paths are returned, totals only: {body}"
    );
    assert!(!body.contains("needle"), "no line text is returned: {body}");
    assert_eq!(structured(&out)["matches"], json!(3));
    assert_eq!(structured(&out)["files"], json!(2));
    assert_eq!(structured(&out)["output"], json!("count"));
}

/// The default (no `output`) is byte-identical to today: `content` changes nothing for a
/// small fixture. Guards the default against silently becoming `files`.
#[tokio::test]
async fn grep_default_output_is_byte_identical_to_content_mode() {
    let dir = TempDir::new("grep-default-identical");
    let root = dir.file("ws");
    dir.write("ws/a.txt", "alpha\nbeta\nAlpha gamma\n");
    let tools = tools(&root);

    let default = call(&tools, "grep", json!({"pattern": "alpha"}))
        .await
        .unwrap();
    let explicit = call(
        &tools,
        "grep",
        json!({"pattern": "alpha", "output": "content"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&default), text(&explicit));
    assert_eq!(text(&default), "a.txt:1:alpha\n");
}

// ---------- context-first byte-cap behavior ----------

/// Over the byte cap with `context`: every match line survives, context lines are dropped
/// first, and the note names the dropped context count. The uncapped run fixes the exact
/// expected context count, so the note pins the number, not just the words.
#[tokio::test]
async fn grep_byte_cap_drops_context_before_matches() {
    let dir = TempDir::new("grep-context-first");
    let root = dir.file("ws");
    // Four files of 50 matches each (the per-file cap): one `needle` line plus 20 filler
    // lines per block, so `context: 10` covers every filler line between matches exactly
    // once, plus 10 after each file's last match.
    for file_no in 0..4 {
        let mut content = String::new();
        for block in 0..50 {
            content.push_str("needle\n");
            for filler in 0..20 {
                content.push_str(&format!("filler-{file_no}-{block}-{filler}\n"));
            }
        }
        dir.write(&format!("ws/f{file_no}.txt"), &content);
    }
    let uncapped = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 8 << 20,
            ..Limits::default()
        },
    );
    let full = call(
        &uncapped,
        "grep",
        json!({"pattern": "needle", "context": 10, "max_matches": 500}),
    )
    .await
    .unwrap();
    let full_body = text(&full);
    assert!(
        !full_body.contains("byte limit"),
        "the uncapped run fits everything: {full_body}"
    );
    let full_matches: Vec<&str> = full_body
        .lines()
        .filter(|line| line.contains(".txt:"))
        .collect();
    assert_eq!(full_matches.len(), 200, "all 200 matches fit uncapped");
    let full_context = full_body
        .lines()
        .filter(|line| line.contains(".txt-"))
        .count();
    assert!(full_context > 3000, "context dominates: {full_context}");

    let capped = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 8192,
            ..Limits::default()
        },
    );
    let out = call(
        &capped,
        "grep",
        json!({"pattern": "needle", "context": 10, "max_matches": 500}),
    )
    .await
    .unwrap();
    let body = text(&out);
    assert!(
        body.len() <= 8192,
        "output {}B passes the 8192B cap",
        body.len()
    );
    for line in &full_matches {
        assert!(
            body.contains(*line),
            "every match line survives, missing: {line}"
        );
    }
    assert!(
        !body.lines().any(|line| line.contains(".txt-")),
        "context lines were dropped first: {body}"
    );
    assert!(
        body.contains(&format!("dropped {full_context} context lines")),
        "the note names the dropped context count: {body}"
    );
    assert_eq!(structured(&out)["matches"], json!(200));
}

/// Match lines alone over the byte cap: matches are cut whole lines in order, context is
/// gone, and the note names both dropped counts. Whole lines: every shown match line is
/// byte-identical to its uncapped run.
#[tokio::test]
async fn grep_byte_cap_cuts_matches_only_after_context_is_gone() {
    let dir = TempDir::new("grep-matches-over-cap");
    let root = dir.file("ws");
    // Twenty 500-byte match lines with two short filler lines between them; `context: 5`
    // shows every filler line, but the matches alone (~10 KiB) already pass a 2 KiB cap.
    let long = format!("needle-{}", "y".repeat(490));
    let mut content = String::new();
    for block in 0..20 {
        content.push_str(&long);
        content.push('\n');
        if block < 19 {
            content.push_str("gap-a\ngap-b\n");
        }
    }
    dir.write("ws/f.txt", &content);
    let uncapped = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 8 << 20,
            ..Limits::default()
        },
    );
    let full = call(
        &uncapped,
        "grep",
        json!({"pattern": "needle", "context": 5}),
    )
    .await
    .unwrap();
    let full_body = text(&full);
    let full_matches: Vec<&str> = full_body
        .lines()
        .filter(|line| line.contains(".txt:"))
        .collect();
    assert_eq!(full_matches.len(), 20);
    let full_context = full_body
        .lines()
        .filter(|line| line.contains(".txt-"))
        .count();
    assert!(full_context > 0, "the fixture has context to drop");

    let capped = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 2048,
            ..Limits::default()
        },
    );
    let out = call(&capped, "grep", json!({"pattern": "needle", "context": 5}))
        .await
        .unwrap();
    let body = text(&out);
    assert!(
        body.len() <= 2048,
        "output {}B passes the 2048B cap",
        body.len()
    );
    let shown: Vec<&str> = body.lines().filter(|line| line.contains(".txt:")).collect();
    assert!(!shown.is_empty(), "at least one match is kept: {body}");
    assert!(shown.len() < 20, "matches were cut: {body}");
    // In order, whole lines: the kept matches are the first N uncapped match lines.
    assert_eq!(
        shown,
        &full_matches[..shown.len()],
        "matches are cut whole lines, in order"
    );
    assert!(
        !body.lines().any(|line| line.contains(".txt-")),
        "no context survives when matches alone pass the cap: {body}"
    );
    assert!(
        body.contains(&format!(
            "dropped {full_context} context lines and {} matches",
            20 - shown.len()
        )),
        "the note names both dropped counts: {body}"
    );
}

/// The tool description points "which files contain X" at `output: "files"` and "how many
/// matches" at `output: "count"`.
#[tokio::test]
async fn grep_description_points_files_and_count_at_their_questions() {
    let dir = TempDir::new("grep-description");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let session = tools(&root).open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "grep")
        .expect("the grep spec");
    assert!(
        spec.description
            .contains("`output: \"files\"` for which files"),
        "the description points file questions at files: {}",
        spec.description
    );
    assert!(
        spec.description
            .contains("`output: \"count\"` for how many"),
        "the description points count questions at count: {}",
        spec.description
    );
}

/// The memory bound without measuring memory: a fixture whose full body (matches plus
/// `context: 30` over long filler lines) is many times the byte cap still returns within
/// the cap with exact counts. No retained-size counter is asserted: exposing one would
/// need test-only plumbing in production code (an allocation tracker for the two
/// buffers), which is not natural here — the bound follows from construction (at most
/// two cap-bounded buffers plus one file's scan), and the test pins the observable
/// half (output size plus exact dropped/match counts).
#[tokio::test]
async fn grep_byte_cap_stays_bounded_when_full_body_is_many_times_the_cap() {
    let dir = TempDir::new("grep-bounded-memory");
    let root = dir.file("ws");
    // 8 files x 10 matches; each match followed by 60 ~950B filler lines so
    // `context: 30` shows every filler (30 after + 30 before the next match, plus 30
    // after each file's last match). Full body is several MB against an 8 KiB cap,
    // while the 80 short match lines alone fit comfortably.
    let filler = |file_no: usize, block: usize, idx: usize| {
        format!("pad-{file_no}-{block}-{idx}-{}\n", "x".repeat(900))
    };
    for file_no in 0..8 {
        let mut content = String::new();
        for block in 0..10 {
            content.push_str("needle\n");
            for idx in 0..60 {
                content.push_str(&filler(file_no, block, idx));
            }
        }
        dir.write(&format!("ws/f{file_no}.txt"), &content);
    }
    let uncapped = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 16 << 20,
            ..Limits::default()
        },
    );
    let full = call(
        &uncapped,
        "grep",
        json!({"pattern": "needle", "context": 30}),
    )
    .await
    .unwrap();
    let full_body = text(&full);
    assert!(
        !full_body.contains("byte limit"),
        "the uncapped run fits everything"
    );
    let full_matches: Vec<&str> = full_body
        .lines()
        .filter(|line| line.contains(".txt:"))
        .collect();
    assert_eq!(full_matches.len(), 80, "all 80 matches fit uncapped");
    let full_context = full_body
        .lines()
        .filter(|line| line.contains(".txt-"))
        .count();
    assert!(
        full_body.len() > 10 * 8192,
        "the full body {}B is many times the 8192B cap",
        full_body.len()
    );

    let capped = tools_with(
        &root,
        Limits {
            max_grep_output_bytes: 8192,
            ..Limits::default()
        },
    );
    let out = call(&capped, "grep", json!({"pattern": "needle", "context": 30}))
        .await
        .unwrap();
    let body = text(&out);
    assert!(
        body.len() <= 8192,
        "output {}B passes the 8192B cap",
        body.len()
    );
    for line in &full_matches {
        assert!(body.contains(*line), "every match line survives: {line}");
    }
    assert!(
        !body.lines().any(|line| line.contains(".txt-")),
        "context lines were dropped first"
    );
    assert!(
        body.contains(&format!("dropped {full_context} context lines")),
        "the note names the exact dropped context count: {body}"
    );
    assert_eq!(structured(&out)["matches"], json!(80));
    assert_eq!(structured(&out)["files"], json!(8));
}

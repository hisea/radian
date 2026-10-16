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

/// ADR 0030 O3: `.radian-worktrees/` (one full worktree per task agent) is pruned **structurally**,
/// not by a `.gitignore` coincidence: even with no ignore file, the walk never descends into it.
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

    let failure = call(&tools, "grep", json!({"pattern": "hit", "context": 99}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
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

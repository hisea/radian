//! The `read` tool's line addressing (D25): `offset`/`limit`, line-boundary truncation, and the
//! `next_offset` continuation. `read` is one of the few tools that streams a file, so this is
//! where a large file must be readable in sections instead of re-reading the prefix.

mod common;

use common::{TempDir, call, structured, text, tools};
use radian_coding::CodingTools;
use serde_json::json;

/// Strips the `N: ` line-number prefix `read` adds, so a test can compare the body with the
/// file. A line with no numeric prefix is the continuation hint and is dropped.
fn unbuffer(content: &str) -> String {
    let mut out = String::new();
    for segment in content.split_inclusive('\n') {
        let (line, newline) = match segment.strip_suffix('\n') {
            Some(line) => (line, true),
            None => (segment, false),
        };
        let Some(body) = strip_number(line) else {
            continue; // the hint line: not file content
        };
        out.push_str(body);
        if newline {
            out.push('\n');
        }
    }
    out
}

/// `Some(body)` for a `N: body` line, `None` when the line has no numeric prefix.
fn strip_number(line: &str) -> Option<&str> {
    let (digits, rest) = line.split_once(": ")?;
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Some(rest)
    } else {
        None
    }
}

/// Reads the whole file by following `next_offset`, returning exactly the bytes each call
/// returned (the `N: ` prefixes stripped, before the human-readable continuation hint).
async fn read_whole(tools: &CodingTools, path: &str, max_bytes: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset = 1i64;
    loop {
        let output = call(
            tools,
            "read",
            json!({"path": path, "max_bytes": max_bytes, "offset": offset}),
        )
        .await
        .unwrap();
        let info = structured(&output);
        out.extend_from_slice(unbuffer(&text(&output)).as_bytes());
        match info.get("next_offset").and_then(|value| value.as_i64()) {
            Some(next) => offset = next,
            None => break,
        }
        assert!(out.len() < 1_000_000, "the read loop did not terminate");
    }
    out
}

/// Every returned line carries its **file** line number as `N: `, so a read starting at
/// `offset` numbers the first line `offset` (not `1`). The continuation hint is not a file
/// line and carries no prefix, and `bytes_read` counts the file bytes, not the prefixes.
#[tokio::test]
async fn read_prefixes_each_line_with_its_file_line_number() {
    let dir = TempDir::new("read-numbers");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();

    // From the start: lines 1..3, and the budget cut names line 4.
    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 12}))
        .await
        .unwrap();
    assert_eq!(
        text(&output),
        "1: alpha\n2: beta\n[truncated: continue with offset=3]"
    );
    // bytes_read is the file bytes ("alpha\nbeta\n" = 11), not the 14 output bytes.
    assert_eq!(structured(&output)["bytes_read"], 11);

    // A middle read numbers by the file line, and the hint keeps no number.
    let middle = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": 3, "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&middle),
        "3: gamma\n[truncated: continue with offset=4]"
    );
}

#[tokio::test]
async fn a_missing_read_suggests_a_similar_name_from_the_same_directory() {
    let dir = TempDir::new("read-suggest");
    let root = dir.file("ws");
    // The requested directory has the close name; the workspace root has the *same* close
    // name (`README.md`), which must NOT be offered: only the requested directory is listed.
    dir.write("ws/deep/readme.md", "deep");
    dir.write("ws/README.md", "root");
    dir.write("ws/other/unrelated.txt", "x");
    let tools = tools(&root);

    let failure = call(&tools, "read", json!({"path": "deep/redme.md"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "not_found", "{failure:?}");
    assert!(
        failure.message.contains("Did you mean `deep/readme.md`"),
        "the suggestion is in the requested directory: {}",
        failure.message
    );
    assert!(
        !failure.message.contains("`README.md`"),
        "a name in another directory is never suggested: {}",
        failure.message
    );

    // A name with nothing close keeps the plain not-found and offers nothing.
    let failure = call(&tools, "read", json!({"path": "deep/zzzzzzzz.txt"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "not_found", "{failure:?}");
    assert!(
        !failure.message.contains("Did you mean"),
        "{}",
        failure.message
    );
}

#[tokio::test]
async fn read_starts_at_a_middle_line() {
    let dir = TempDir::new("read-offset");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "offset": 2}))
        .await
        .unwrap();
    assert_eq!(text(&output), "2: two\n3: three\n4: four\n");
    let info = structured(&output);
    assert_eq!(info["start_line"], 2);
    assert_eq!(info["end_line"], 4);
    assert_eq!(info["truncated"], false);
    assert!(info.get("next_offset").is_none(), "{info}");
}

#[tokio::test]
async fn read_limit_stops_after_n_lines() {
    let dir = TempDir::new("read-limit");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "a\nb\nc\nd\n").unwrap();

    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": 2, "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&output),
        "2: b\n3: c\n[truncated: continue with offset=4]"
    );
    let info = structured(&output);
    assert_eq!(info["start_line"], 2);
    assert_eq!(info["end_line"], 3);
    assert_eq!(info["next_offset"], 4);
    assert_eq!(info["truncated"], true);
    assert_eq!(info["line_truncated"], false);
}

#[tokio::test]
async fn a_budget_cut_mid_line_resumes_at_that_line() {
    let dir = TempDir::new("read-boundary");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "aaa\nbbb\nccc\n").unwrap();

    // 4 bytes fits "aaa\n" exactly; "bbb\n" would need 8, so it is dropped whole and the next
    // read resumes at line 2.
    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 4}))
        .await
        .unwrap();
    let info = structured(&output);
    assert_eq!(text(&output), "1: aaa\n[truncated: continue with offset=2]");
    assert_eq!(info["bytes_read"], 4);
    assert_eq!(info["start_line"], 1);
    assert_eq!(info["end_line"], 1);
    assert_eq!(info["next_offset"], 2);
    assert_eq!(info["truncated"], true);
    assert_eq!(info["line_truncated"], false);
}

#[tokio::test]
async fn next_offset_reads_the_whole_file() {
    let dir = TempDir::new("read-loop");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    for (name, content) in [
        ("lf.txt", "alpha\nbeta\ngamma\ndelta\n"),
        // The last line has no newline: the loop must still reproduce the exact bytes.
        ("no-final-newline.txt", "alpha\nbeta\ngamma\ndelta"),
        // CRLF: the `\r` is part of the line and must round-trip.
        ("crlf.txt", "alpha\r\nbeta\r\ngamma\r\n"),
    ] {
        std::fs::write(root.join(name), content).unwrap();
        let original = std::fs::read(root.join(name)).unwrap();
        // Every budget is at least the longest line, so no line is truncated; the loop still
        // stops at line boundaries and follows `next_offset` to the end.
        for budget in [7u64, 8, 13, 64] {
            let read_back = read_whole(&tools, name, budget).await;
            assert_eq!(
                read_back,
                original,
                "{name} at max_bytes={budget}: {:?}",
                String::from_utf8_lossy(&read_back)
            );
        }
    }
}

#[tokio::test]
async fn an_overlong_first_line_is_returned_as_a_marked_prefix() {
    let dir = TempDir::new("read-long-line");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "a\n0123456789\nb\n").unwrap();

    // Start at the long line (line 2) with a 4-byte budget: its prefix is returned, marked
    // `line_truncated`, and the next read resumes at line 3.
    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": 2, "max_bytes": 4}),
    )
    .await
    .unwrap();
    let info = structured(&output);
    let content = text(&output);
    assert!(content.starts_with("2: 0123"), "{content}");
    assert!(
        content.contains("[line 2 truncated at max_bytes; continue with offset=3]"),
        "{content}"
    );
    assert_eq!(info["start_line"], 2);
    assert_eq!(info["end_line"], 2);
    assert_eq!(info["line_truncated"], true);
    assert_eq!(info["next_offset"], 3);
    assert_eq!(info["bytes_read"], 4);

    // And the continuation reads the rest.
    let rest = call(&tools, "read", json!({"path": "f.txt", "offset": 3}))
        .await
        .unwrap();
    assert_eq!(text(&rest), "3: b\n");
    assert_eq!(structured(&rest)["line_truncated"], false);
}

#[tokio::test]
async fn an_offset_past_the_end_is_empty_with_a_note() {
    let dir = TempDir::new("read-past-end");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "a\nb\n").unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "offset": 5}))
        .await
        .unwrap();
    let content = text(&output);
    assert!(content.contains("past the end"), "{content}");
    let info = structured(&output);
    assert_eq!(info["bytes_read"], 0);
    assert_eq!(info["truncated"], false);
    assert!(info.get("next_offset").is_none(), "{info}");

    // An empty file reports the same thing.
    std::fs::write(root.join("empty"), "").unwrap();
    let empty = call(&tools, "read", json!({"path": "empty", "offset": 1}))
        .await
        .unwrap();
    assert!(text(&empty).contains("past the end"), "{}", text(&empty));
}

#[tokio::test]
async fn a_zero_offset_or_a_non_positive_limit_is_invalid_arguments() {
    let dir = TempDir::new("read-bad-args");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "a\n").unwrap();

    for arguments in [
        json!({"path": "f.txt", "offset": 0}),
        json!({"path": "f.txt", "limit": 0}),
        json!({"path": "f.txt", "limit": -3}),
    ] {
        let failure = call(&tools, "read", arguments.clone()).await.unwrap_err();
        assert_eq!(
            failure.code, "invalid_arguments",
            "{arguments}: {failure:?}"
        );
    }
}

fn thousand_lines() -> String {
    let mut out = String::new();
    for n in 1..=1000 {
        out.push_str(&format!("line {n}\n"));
    }
    out
}

/// `offset: -5` on a 1000-line file returns lines 996–1000 with their file numbers.
#[tokio::test]
async fn a_negative_offset_reads_the_last_lines_with_file_numbers() {
    let dir = TempDir::new("read-tail");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "offset": -5}))
        .await
        .unwrap();
    let mut expected = String::new();
    for n in 996..=1000 {
        expected.push_str(&format!("{n}: line {n}\n"));
    }
    assert_eq!(text(&output), expected);
    let info = structured(&output);
    assert_eq!(info["start_line"], 996);
    assert_eq!(info["end_line"], 1000);
    assert!(info.get("next_offset").is_none(), "{info}");
}

/// `limit` and `next_offset` work as for a positive offset, with absolute line numbers.
#[tokio::test]
async fn a_negative_offset_with_a_limit_names_the_absolute_next_offset() {
    let dir = TempDir::new("read-tail-limit");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": -5, "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&output),
        "996: line 996\n997: line 997\n[truncated: continue with offset=998]"
    );
    let info = structured(&output);
    assert_eq!(info["start_line"], 996);
    assert_eq!(info["end_line"], 997);
    assert_eq!(info["next_offset"], 998);
}

/// A negative `offset` past the start reads from line 1; an empty file is an empty read.
#[tokio::test]
async fn a_negative_offset_past_the_start_reads_from_line_one() {
    let dir = TempDir::new("read-tail-clamp");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "offset": -2000}))
        .await
        .unwrap();
    let info = structured(&output);
    assert_eq!(info["start_line"], 1);
    assert!(
        text(&output).starts_with("1: line 1\n"),
        "{}",
        text(&output)
    );

    std::fs::write(root.join("empty"), "").unwrap();
    let empty = call(&tools, "read", json!({"path": "empty", "offset": -3}))
        .await
        .unwrap();
    assert_eq!(structured(&empty)["bytes_read"], 0);
}

/// `offset: -1` is the last line, with or without a trailing newline: the count treats a
/// non-empty tail without `\\n` as one more line, matching what a forward read shows.
#[tokio::test]
async fn a_negative_one_is_the_last_line_with_or_without_a_trailing_newline() {
    let dir = TempDir::new("read-tail-newline");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("lf.txt"), "a\nb\nc\n").unwrap();
    std::fs::write(root.join("no-lf.txt"), "a\nb\nc").unwrap();

    for (name, expected) in [("lf.txt", "3: c\n"), ("no-lf.txt", "3: c")] {
        let output = call(&tools, "read", json!({"path": name, "offset": -1}))
            .await
            .unwrap();
        assert_eq!(text(&output), expected, "{name}: {}", text(&output));
        let info = structured(&output);
        assert_eq!(info["start_line"], 3, "{name}: {info}");
        assert_eq!(info["end_line"], 3, "{name}: {info}");
    }
}

/// A file larger than `max_bytes` read from the end stays within the budget.
#[tokio::test]
async fn a_negative_offset_on_a_file_larger_than_the_budget_stays_bounded() {
    let dir = TempDir::new("read-tail-budget");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();
    let total = std::fs::metadata(root.join("f.txt")).unwrap().len();
    assert!(total > 64, "the file must exceed the budget");

    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": -5, "max_bytes": 64}),
    )
    .await
    .unwrap();
    let info = structured(&output);
    assert!(info["bytes_read"].as_u64().unwrap() <= 64, "{info}");
    assert_eq!(info["start_line"], 996);
}

/// A negative `offset` on a directory is ignored like a positive one.
#[tokio::test]
async fn a_negative_offset_on_a_directory_is_ignored() {
    let dir = TempDir::new("read-tail-dir");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/f.txt"), "x\n").unwrap();
    let tools = tools(&root);

    let output = call(&tools, "read", json!({"path": "sub", "offset": -5}))
        .await
        .unwrap();
    assert!(
        text(&output).contains("ignored for this directory"),
        "{}",
        text(&output)
    );
}

#[tokio::test]
async fn a_multibyte_character_on_the_budget_edge_is_not_split() {
    let dir = TempDir::new("read-multibyte");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    // "é" is 2 bytes; the line "é\n" is 3 bytes.
    std::fs::write(root.join("f.txt"), "é\nnext\n").unwrap();

    // A 2-byte budget drops the incomplete "\n" but keeps the whole character.
    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 2}))
        .await
        .unwrap();
    let info = structured(&output);
    let content = text(&output);
    assert!(content.starts_with("1: é"), "{content}");
    assert!(
        content.contains("[line 1 truncated at max_bytes; continue with offset=2]"),
        "{content}"
    );
    assert_eq!(info["line_truncated"], true);
    assert_eq!(info["next_offset"], 2);
    assert_eq!(info["bytes_read"], 2);

    // A 1-byte budget cannot even hold the character: the prefix is empty and only the hint
    // remains.
    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 1}))
        .await
        .unwrap();
    let content = text(&output);
    assert!(
        !content.contains('é'),
        "the character is not split: {content}"
    );
    assert!(
        content.contains("[line 1 truncated at max_bytes; continue with offset=2]"),
        "{content}"
    );
    assert_eq!(structured(&output)["bytes_read"], 0);
}

#[tokio::test]
async fn a_budget_ending_exactly_on_a_character_keeps_it() {
    let dir = TempDir::new("read-exact");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "é\né\n").unwrap();

    // 3 bytes is exactly the first line "é\n".
    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 3}))
        .await
        .unwrap();
    assert_eq!(text(&output), "1: é\n[truncated: continue with offset=2]");
    assert_eq!(structured(&output)["end_line"], 1);
}

/// A 64 MiB file with **no newline** (a minified bundle, a huge log line): the old `read_until`
/// implementation would pull the whole line into memory (the OOM case). `read` must return a
/// bounded prefix plus the continuation hint, and starting past that one line must be a cheap
/// past-the-end note. This does not measure memory; it is the shape of the OOM scenario.
#[tokio::test]
async fn a_huge_single_line_is_bounded() {
    let dir = TempDir::new("read-huge");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    let path = root.join("huge.js");
    // Sparse: 64 MiB of zero bytes, no newline anywhere.
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    drop(file);

    let output = call(
        &tools,
        "read",
        json!({"path": "huge.js", "max_bytes": 1024}),
    )
    .await
    .unwrap();
    let info = structured(&output);
    assert_eq!(info["line_truncated"], true);
    assert_eq!(info["next_offset"], 2);
    assert_eq!(info["bytes_read"], 1024);
    assert!(
        text(&output).contains("continue with offset=2"),
        "{}",
        text(&output)
    );

    // Line 2 does not exist; the skip scans to EOF with constant memory.
    let past = call(&tools, "read", json!({"path": "huge.js", "offset": 2}))
        .await
        .unwrap();
    assert!(text(&past).contains("past the end"), "{}", text(&past));
    assert_eq!(structured(&past)["bytes_read"], 0);
}

/// `lines` lines of `width` bytes each (the last one a `\n`).
fn lines_of(lines: usize, width: usize) -> String {
    let body = "x".repeat(width - 1);
    let mut out = String::new();
    for _ in 0..lines {
        out.push_str(&body);
        out.push('\n');
    }
    out
}

/// Defaults, case 1: a file over 16 KiB with fewer than 200 lines is cut at the byte budget,
/// on a line boundary, and the result names the next line.
#[tokio::test]
async fn the_default_byte_budget_cuts_at_a_line_boundary_and_names_the_next_line() {
    let dir = TempDir::new("read-default-bytes");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    // 150 lines of 120 bytes: 18,000 bytes (over 16 KiB) and well under 200 lines.
    std::fs::write(root.join("f.txt"), lines_of(150, 120)).unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt"}))
        .await
        .unwrap();
    let info = structured(&output);
    assert_eq!(info["start_line"], 1, "{info}");
    let end = info["end_line"].as_i64().expect("an end line");
    assert!(end < 150, "the byte budget cut the file: {info}");
    assert!(
        info["bytes_read"].as_u64().unwrap() <= 16 * 1024,
        "at most the 16 KiB budget: {info}"
    );
    assert_eq!(
        info["bytes_read"],
        json!(end * 120),
        "the cut is on a line boundary: {info}"
    );
    assert_eq!(info["next_offset"], json!(end + 1), "{info}");
    let content = text(&output);
    assert!(
        content.ends_with(&format!("[truncated: continue with offset={}]", end + 1)),
        "the result names the next offset:\n{content}"
    );
    assert!(
        !content.contains("truncated at max_bytes"),
        "a whole line was dropped, not split:\n{content}"
    );
}

/// Defaults, case 2: a file under 16 KiB with more than 200 lines stops after line 200 and
/// names line 201.
#[tokio::test]
async fn the_default_line_limit_stops_after_two_hundred_lines() {
    let dir = TempDir::new("read-default-lines");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    // 250 lines of "x\n": 500 bytes, so only the line cap can stop this read.
    std::fs::write(root.join("f.txt"), lines_of(250, 2)).unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt"}))
        .await
        .unwrap();
    let info = structured(&output);
    assert_eq!(info["start_line"], 1, "{info}");
    assert_eq!(info["end_line"], 200, "{info}");
    assert_eq!(info["bytes_read"], 400, "{info}");
    assert_eq!(info["next_offset"], 201, "{info}");
    assert_eq!(info["truncated"], true, "{info}");
    assert!(
        text(&output).ends_with("[truncated: continue with offset=201]"),
        "{}",
        text(&output)
    );

    // A file that ends exactly on line 200 is not cut: there is no next line, so no note.
    std::fs::write(root.join("exact.txt"), lines_of(200, 2)).unwrap();
    let exact = call(&tools, "read", json!({"path": "exact.txt"}))
        .await
        .unwrap();
    let info = structured(&exact);
    assert_eq!(info["end_line"], 200, "{info}");
    assert!(info.get("next_offset").is_none(), "{info}");
    assert_eq!(info["truncated"], false, "{info}");
}

/// Defaults, case 3: an explicit `max_bytes: 65536` and `limit: 5000` still reach the old
/// 64 KiB window on request, on top of a 70 KB file.
#[tokio::test]
async fn an_explicit_budget_and_limit_restore_the_old_64_kib_window() {
    let dir = TempDir::new("read-old-window");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    // 1400 lines of 50 bytes: 70,000 bytes.
    std::fs::write(root.join("f.txt"), lines_of(1400, 50)).unwrap();

    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "max_bytes": 65536, "limit": 5000}),
    )
    .await
    .unwrap();
    let info = structured(&output);
    let end = info["end_line"].as_i64().expect("an end line");
    assert!(end < 1400, "the 64 KiB budget still cuts the file: {info}");
    assert_eq!(info["bytes_read"], json!(end * 50), "{info}");
    assert!(
        info["bytes_read"].as_u64().unwrap() > 16 * 1024,
        "the explicit budget is 64 KiB, not the 16 KiB default: {info}"
    );
    assert!(info["bytes_read"].as_u64().unwrap() <= 65536, "{info}");
    assert_eq!(info["next_offset"], json!(end + 1), "{info}");
    assert!(
        text(&output).contains(&format!("continue with offset={}", end + 1)),
        "{}",
        text(&output)
    );
}

/// Defaults, case 4: an explicit range is the caller's own: `offset: 100, limit: 50` returns
/// exactly those 50 lines and keeps today's continuation note for line 150.
#[tokio::test]
async fn an_explicit_offset_and_limit_are_unchanged_by_the_defaults() {
    let dir = TempDir::new("read-explicit-range");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": 100, "limit": 50}),
    )
    .await
    .unwrap();
    let info = structured(&output);
    assert_eq!(info["start_line"], 100, "{info}");
    assert_eq!(info["end_line"], 149, "{info}");
    assert_eq!(info["next_offset"], 150, "{info}");
    let content = text(&output);
    assert!(content.starts_with("100: line 100\n"), "{content}");
    assert!(
        content.ends_with("[truncated: continue with offset=150]"),
        "the explicit range keeps its continuation note:\n{content}"
    );
}

/// An explicit `limit` that reaches the end of the file is not a cut: the result carries
/// no note and no `next_offset`.
#[tokio::test]
async fn an_explicit_limit_that_reaches_the_end_of_the_file_has_no_note() {
    let dir = TempDir::new("read-explicit-range-end");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let output = call(
        &tools,
        "read",
        json!({"path": "f.txt", "offset": 990, "limit": 50}),
    )
    .await
    .unwrap();
    let info = structured(&output);
    assert_eq!(info["start_line"], 990, "{info}");
    assert_eq!(info["end_line"], 1000, "{info}");
    assert!(info.get("next_offset").is_none(), "{info}");
    assert_eq!(info["truncated"], false, "{info}");
    let content = text(&output);
    assert!(content.starts_with("990: line 990\n"), "{content}");
    assert!(content.ends_with("1000: line 1000\n"), "{content}");
    assert!(
        !content.contains("[truncated"),
        "a range that reaches the end carries no note:\n{content}"
    );
}

/// Defaults, case 5: `offset: 100` with no `limit` still gets 200 lines, then names line 300.
#[tokio::test]
async fn the_default_line_limit_applies_after_a_positive_offset() {
    let dir = TempDir::new("read-default-offset");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let output = call(&tools, "read", json!({"path": "f.txt", "offset": 100}))
        .await
        .unwrap();
    let info = structured(&output);
    assert_eq!(info["start_line"], 100, "{info}");
    assert_eq!(info["end_line"], 299, "{info}");
    assert_eq!(info["next_offset"], 300, "{info}");
    assert!(
        text(&output).ends_with("[truncated: continue with offset=300]"),
        "{}",
        text(&output)
    );
}

/// Defaults, case 6: a negative offset still counts from the end. A short tail (`-50`) fits
/// under 200 lines and has no note; a long one (`-300`) stops after 200 lines and names the
/// next.
#[tokio::test]
async fn a_negative_offset_uses_the_default_line_limit_only_past_two_hundred_lines() {
    let dir = TempDir::new("read-default-tail");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), thousand_lines()).unwrap();

    let short = call(&tools, "read", json!({"path": "f.txt", "offset": -50}))
        .await
        .unwrap();
    let info = structured(&short);
    assert_eq!(info["start_line"], 951, "{info}");
    assert_eq!(info["end_line"], 1000, "{info}");
    assert!(info.get("next_offset").is_none(), "{info}");
    assert!(
        !text(&short).contains("[truncated"),
        "a 50-line tail is not cut:\n{}",
        text(&short)
    );

    let long = call(&tools, "read", json!({"path": "f.txt", "offset": -300}))
        .await
        .unwrap();
    let info = structured(&long);
    assert_eq!(info["start_line"], 701, "{info}");
    assert_eq!(info["end_line"], 900, "{info}");
    assert_eq!(info["next_offset"], 901, "{info}");
    assert!(
        text(&long).ends_with("[truncated: continue with offset=901]"),
        "{}",
        text(&long)
    );
}

//! The `read` tool's line addressing (D25): `offset`/`limit`, line-boundary truncation, and the
//! `next_offset` continuation. `read` is one of the few tools that streams a file, so this is
//! where a large file must be readable in sections instead of re-reading the prefix.

mod common;

use common::{TempDir, call, structured, text, tools};
use radian_coding::CodingTools;
use serde_json::json;

/// Reads the whole file by following `next_offset`, returning exactly the bytes each call
/// returned (`bytes_read` bytes of the content, before the human-readable continuation hint).
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
        let content = text(&output);
        let bytes_read = info["bytes_read"].as_u64().unwrap() as usize;
        out.extend_from_slice(&content.as_bytes()[..bytes_read]);
        match info.get("next_offset").and_then(|value| value.as_i64()) {
            Some(next) => offset = next,
            None => break,
        }
        assert!(out.len() < 1_000_000, "the read loop did not terminate");
    }
    out
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
    assert_eq!(text(&output), "two\nthree\nfour\n");
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
    assert_eq!(text(&output), "b\nc\n[truncated: continue with offset=4]");
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
    assert_eq!(text(&output), "aaa\n[truncated: continue with offset=2]");
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
    assert_eq!(text(&output), "0123");
    assert_eq!(info["start_line"], 2);
    assert_eq!(info["end_line"], 2);
    assert_eq!(info["line_truncated"], true);
    assert_eq!(info["next_offset"], 3);
    assert_eq!(info["bytes_read"], 4);

    // And the continuation reads the rest.
    let rest = call(&tools, "read", json!({"path": "f.txt", "offset": 3}))
        .await
        .unwrap();
    assert_eq!(text(&rest), "b\n");
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
    assert!(content.contains("2 line"), "{content}");
    let info = structured(&output);
    assert_eq!(info["bytes_read"], 0);
    assert_eq!(info["truncated"], false);
    assert!(info.get("next_offset").is_none(), "{info}");

    // An empty file reports that too.
    std::fs::write(root.join("empty"), "").unwrap();
    let empty = call(&tools, "read", json!({"path": "empty", "offset": 1}))
        .await
        .unwrap();
    assert!(
        text(&empty).contains("the file is empty"),
        "{}",
        text(&empty)
    );
}

#[tokio::test]
async fn a_non_positive_offset_or_limit_is_invalid_arguments() {
    let dir = TempDir::new("read-bad-args");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);
    std::fs::write(root.join("f.txt"), "a\n").unwrap();

    for arguments in [
        json!({"path": "f.txt", "offset": 0}),
        json!({"path": "f.txt", "offset": -1}),
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
    assert_eq!(text(&output), "é");
    assert_eq!(info["line_truncated"], true);
    assert_eq!(info["next_offset"], 2);
    assert_eq!(info["bytes_read"], 2);

    // A 1-byte budget cannot even hold the character: the prefix is empty.
    let output = call(&tools, "read", json!({"path": "f.txt", "max_bytes": 1}))
        .await
        .unwrap();
    assert_eq!(text(&output), "");
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
    assert_eq!(text(&output), "é\n[truncated: continue with offset=2]");
    assert_eq!(structured(&output)["end_line"], 1);
}

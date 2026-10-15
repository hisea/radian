//! `patch`'s layered matching (piece 3 of the tool-details slice). The layers exist only to
//! *locate* text: the replacement is always `new`, the replaced bytes are the file's own bytes,
//! and a layer that matches more than once is still refused as ambiguous.

mod common;

use common::{TempDir, call, structured, tools};
use serde_json::json;

/// Layer 2: typographic quotes, dashes and no-break spaces on either side still locate the
/// text, and the file's own (non-ASCII) bytes are what get replaced.
#[tokio::test]
async fn a_typographic_variant_is_located_and_replaced() {
    let dir = TempDir::new("patch-typographic");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    for (name, content, old, new, expected) in [
        (
            "quotes.txt",
            "say \u{201C}hello\u{201D} now\n",
            "\"hello\"",
            "\"bye\"",
            "say \"bye\" now\n",
        ),
        ("dashes.txt", "a \u{2014} b\n", "a - b", "a + b", "a + b\n"),
        ("spaces.txt", "a\u{00A0}b\n", "a b", "a_b", "a_b\n"),
    ] {
        std::fs::write(root.join(name), content).unwrap();
        let output = call(
            &tools,
            "patch",
            json!({"path": name, "old": old, "new": new}),
        )
        .await
        .unwrap();
        assert_eq!(structured(&output)["matched"], "typographic normalisation");
        assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), expected);
    }
}

/// A layer locates only; it never guesses. Two normalised matches are still ambiguous.
#[tokio::test]
async fn a_normalised_match_must_still_be_unique() {
    let dir = TempDir::new("patch-typographic-ambiguous");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "\u{201C}x\u{201D}\n\u{201C}x\u{201D}\n").unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "\"x\"", "new": "y"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_ambiguous", "{failure:?}");
    assert!(failure.message.contains("2 times"), "{failure:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "\u{201C}x\u{201D}\n\u{201C}x\u{201D}\n",
        "an ambiguous match changes nothing"
    );
}

/// Layer 3: whole lines agree once trailing whitespace is stripped, and the block's interior
/// newlines are part of the replaced range.
#[tokio::test]
async fn trailing_whitespace_is_ignored_line_by_line() {
    let dir = TempDir::new("patch-lines");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "foo   \nbar\t\nbaz\n").unwrap();
    let tools = tools(&root);

    let output = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "foo\nbar", "new": "FOO\nBAR"}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["matched"],
        "line-by-line, trailing whitespace ignored"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "FOO\nBAR\nbaz\n"
    );
}

/// The line layer is unique too: two matching blocks are ambiguous.
#[tokio::test]
async fn a_line_match_must_still_be_unique() {
    let dir = TempDir::new("patch-lines-ambiguous");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "foo  \nbar\nfoo \nbar\n").unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "foo\nbar", "new": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_ambiguous", "{failure:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "foo  \nbar\nfoo \nbar\n"
    );
}

/// `replace_all` replaces every occurrence the winning layer located.
#[tokio::test]
async fn replace_all_replaces_every_located_match() {
    let dir = TempDir::new("patch-replace-all");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "x\nx\nx\n").unwrap();
    let tools = tools(&root);

    // Without replace_all the three matches are ambiguous and nothing changes.
    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "x", "new": "y"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_ambiguous", "{failure:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "x\nx\nx\n"
    );

    let output = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "x", "new": "y", "replace_all": true}),
    )
    .await
    .unwrap();
    assert_eq!(structured(&output)["replaced"], 3);
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "y\ny\ny\n"
    );
}

/// A CRLF file is matched on an LF view: an LF needle locates the text, the offsets map back,
/// and the file stays CRLF (the replacement's line endings are converted to CRLF too).
#[tokio::test]
async fn a_crlf_file_matches_an_lf_needle_and_stays_crlf() {
    let dir = TempDir::new("patch-crlf");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();
    let tools = tools(&root);

    let output = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "alpha\nbeta", "new": "ALPHA\nBETA"}),
    )
    .await
    .unwrap();
    assert_eq!(structured(&output)["replaced"], 1);
    let after = std::fs::read(root.join("f.txt")).unwrap();
    assert_eq!(after, b"ALPHA\r\nBETA\r\ngamma\r\n");
    // Every LF is still preceded by a CR: the file did not become mixed.
    assert!(
        after
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .all(|(index, _)| index > 0 && after[index - 1] == b'\r'),
        "no bare LF in a CRLF file: {:?}",
        String::from_utf8_lossy(&after)
    );
}

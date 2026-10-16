//! `patch`'s layered matching (piece 3 of the tool-details slice). The layers exist only to
//! *locate* text: the replacement is always `new`, the replaced bytes are the file's own bytes,
//! and a layer that matches more than once is still refused as ambiguous.

mod common;

use common::{TempDir, call, structured, text, tools};
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

/// A not-found needle names the closest region of the file with line numbers, so the model can
/// fix the needle without another `read`.
#[tokio::test]
async fn a_missing_needle_reports_the_closest_region_with_line_numbers() {
    let dir = TempDir::new("patch-closest");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let tools = tools(&root);

    // Case differs on the second line, so no layer matches.
    let failure = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "beta\nGAMMA", "new": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "patch_not_found", "{failure:?}");
    assert!(
        failure.message.contains("lines 2-3"),
        "the closest region is named: {}",
        failure.message
    );
    assert!(
        failure.message.contains("2: beta") && failure.message.contains("3: gamma"),
        "the region carries line numbers and the file's text: {}",
        failure.message
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "alpha\nbeta\ngamma\ndelta\n"
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

// ---------- the `hunks` field (ADR 0028) ----------

/// One replaced range: the structured result names its 1-based start line and line count in the
/// old file, and the replacement's in the new file. The result **text** is unchanged, and the
/// counts come from `str::lines` (a trailing newline ends the last line, it does not add one).
#[tokio::test]
async fn a_hunk_names_the_replaced_range_in_both_files() {
    let dir = TempDir::new("patch-hunks");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    dir.write("ws/f.txt", "one\ntwo\nthree\nfour\n");
    let tools = tools(&root);

    let output = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "two\nthree\n", "new": "TWO\n"}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["hunks"],
        json!([{"old_start": 2, "old_lines": 2, "new_start": 2, "new_lines": 1}]),
    );
    assert_eq!(
        text(&output),
        "replaced 1 occurrence in f.txt (matched by exact)",
        "the text is unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "one\nTWO\nfour\n"
    );
}

/// Several occurrences: one hunk each, in file order, and the line delta of the earlier hunks
/// moves the later ones' **new** file start — a deletion shifts them up, an insertion down.
#[tokio::test]
async fn several_hunks_carry_the_line_delta_of_the_earlier_ones() {
    let dir = TempDir::new("patch-hunks-many");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    // A deletion: two lines become one, so the second hunk starts a line earlier in the new file.
    dir.write("ws/del.txt", "keep\na\nb\nkeep\na\nb\nkeep\n");
    let output = call(
        &tools,
        "patch",
        json!({"path": "del.txt", "old": "a\nb\n", "new": "A\n", "replace_all": true}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["hunks"],
        json!([
            {"old_start": 2, "old_lines": 2, "new_start": 2, "new_lines": 1},
            {"old_start": 5, "old_lines": 2, "new_start": 4, "new_lines": 1},
        ]),
    );
    assert_eq!(
        std::fs::read_to_string(root.join("del.txt")).unwrap(),
        "keep\nA\nkeep\nA\nkeep\n"
    );

    // An insertion: one line becomes two, so the second hunk starts a line later.
    dir.write("ws/ins.txt", "y\nmid\ny\n");
    let output = call(
        &tools,
        "patch",
        json!({"path": "ins.txt", "old": "y\n", "new": "Y1\nY2\n", "replace_all": true}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["hunks"],
        json!([
            {"old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 2},
            {"old_start": 3, "old_lines": 1, "new_start": 4, "new_lines": 2},
        ]),
    );
    assert_eq!(
        std::fs::read_to_string(root.join("ins.txt")).unwrap(),
        "Y1\nY2\nmid\nY1\nY2\n"
    );
}

/// A CRLF file: the hunks are counted on the LF view (the `\r` is not a line of its own), and
/// the file stays CRLF.
#[tokio::test]
async fn a_crlf_file_numbers_its_hunks() {
    let dir = TempDir::new("patch-hunks-crlf");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    dir.write("ws/f.txt", "alpha\r\nbeta\r\ngamma\r\n");
    let tools = tools(&root);

    let output = call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": "beta\n", "new": "BETA\nBETA2\n"}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["hunks"],
        json!([{"old_start": 2, "old_lines": 1, "new_start": 2, "new_lines": 2}]),
    );
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "alpha\r\nBETA\r\nBETA2\r\ngamma\r\n"
    );
}

/// A range at line 1 and a range at the last line (a file with no trailing newline).
#[tokio::test]
async fn a_hunk_at_the_first_and_at_the_last_line() {
    let dir = TempDir::new("patch-hunks-edges");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    dir.write("ws/first.txt", "first\nsecond\n");
    let output = call(
        &tools,
        "patch",
        json!({"path": "first.txt", "old": "first\n", "new": "FIRST\n"}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["hunks"],
        json!([{"old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 1}]),
    );

    // The last line, with no trailing newline: one line, and it is the file's third.
    dir.write("ws/last.txt", "one\ntwo\nlast");
    let output = call(
        &tools,
        "patch",
        json!({"path": "last.txt", "old": "last", "new": "LAST"}),
    )
    .await
    .unwrap();
    assert_eq!(
        structured(&output)["hunks"],
        json!([{"old_start": 3, "old_lines": 1, "new_start": 3, "new_lines": 1}]),
    );
    assert_eq!(
        std::fs::read_to_string(root.join("last.txt")).unwrap(),
        "one\ntwo\nLAST"
    );
}

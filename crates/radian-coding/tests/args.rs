//! Tool argument errors (piece 5). One failure lists every problem, echoes the arguments the
//! model sent, and ends with a fixed instruction; an optional argument sent as `""` is
//! treated as absent.

mod common;

use common::{TempDir, call, text, tools};
use serde_json::json;

#[tokio::test]
async fn an_argument_error_lists_every_problem_and_echoes_the_arguments() {
    let dir = TempDir::new("args-list");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(
        &tools,
        "read",
        json!({"path": 5, "offset": "soon", "extra": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
    for problem in [
        "`path` must be string, got integer",
        "`offset` must be integer, got string",
        "unknown argument `extra`",
    ] {
        assert!(
            failure.message.contains(problem),
            "missing problem {problem:?} in: {}",
            failure.message
        );
    }
    assert!(
        failure.message.contains("arguments received:"),
        "{}",
        failure.message
    );
    assert!(
        failure
            .message
            .ends_with("Update the arguments and call the tool again"),
        "{}",
        failure.message
    );
}

#[tokio::test]
async fn a_missing_required_argument_is_named_and_not_a_serde_error() {
    let dir = TempDir::new("args-required");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "write", json!({"path": "f.txt"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
    assert!(
        failure
            .message
            .contains("missing required argument `content`"),
        "{}",
        failure.message
    );
}

#[tokio::test]
async fn an_empty_optional_argument_is_treated_as_absent() {
    let dir = TempDir::new("args-empty");
    let root = dir.file("ws");
    dir.write("ws/f.txt", "one\ntwo\n");
    dir.write("ws/g.txt", "g\n");
    let tools = tools(&root);

    // read: `offset: ""` is "use the default", so the read starts at line 1.
    let output = call(&tools, "read", json!({"path": "f.txt", "offset": ""}))
        .await
        .unwrap();
    assert_eq!(text(&output), "1: one\n2: two\n");

    // glob: `path: ""` is "the workspace root", not an empty path.
    let output = call(&tools, "glob", json!({"pattern": "*.txt", "path": ""}))
        .await
        .unwrap();
    assert!(text(&output).contains("f.txt"), "{}", text(&output));
}

/// A **required** argument sent as `""` is a real empty value, not "absent": `patch`'s `new`
/// may be empty to delete the matched text.
#[tokio::test]
async fn an_empty_required_argument_is_kept() {
    let dir = TempDir::new("args-required-empty");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "alpha beta gamma\n").unwrap();
    let tools = tools(&root);

    call(
        &tools,
        "patch",
        json!({"path": "f.txt", "old": " beta", "new": ""}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "alpha gamma\n"
    );
}

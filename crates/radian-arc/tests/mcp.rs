//! The shape of the lead's MCP tool results (ADR 0048 decision 11): the text content is the
//! tool's JSON, the same document the CLI prints, and `structuredContent` is set only when
//! that JSON is an object. `list` answers with `radian arc list --json`'s array, so its result
//! is text only: the MCP spec requires `structuredContent` to be an object, and a client that
//! checks the field (Claude Code) rejected the whole result otherwise.
//!
//! Every test builds a throwaway repository with a throwaway bare `origin` under the system
//! temp directory, and calls the tools through [`ArcServer::call`], the same answer
//! `call_tool` returns.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::mcp::ArcServer;
use radian_arc::store::Store;
use radian_arc::store::cli::{list_json, show_json};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-mcp-result-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the parent is creatable");
        }
        fs::write(&path, text).expect("the file is writable");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "arc test")
        .env("GIT_AUTHOR_EMAIL", "arc@example.com")
        .env("GIT_COMMITTER_NAME", "arc test")
        .env("GIT_COMMITTER_EMAIL", "arc@example.com")
        .args(["-c", "core.hooksPath="])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with a bare `origin`, set up for arc.
fn fixture(label: &str) -> Scratch {
    let scratch = Scratch::new(label);
    git(&scratch.dir, &["init", "-q", "--bare", "origin.git"]);
    git(
        &scratch.path("origin.git"),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(&scratch.dir, &["init", "-q", "repo"]);
    git(
        &scratch.repo(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    scratch.write("repo/README.md", "hello\n");
    git(&scratch.repo(), &["add", "README.md"]);
    git(&scratch.repo(), &["commit", "-q", "-m", "first"]);
    let origin = scratch.path("origin.git");
    git(
        &scratch.repo(),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&scratch.repo(), &["push", "-q", "-u", "origin", "main"]);
    git(&scratch.repo(), &["remote", "set-head", "origin", "main"]);
    Store::init(&scratch.repo(), false).expect("arc sets up");
    scratch
}

/// The test's own client-side read of a result: its text blocks, joined as a client sees them.
fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text content parsed as JSON: the document the tool answers with.
fn value_of(result: &CallToolResult) -> Value {
    let text = text_of(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the text content is not JSON ({error}): {text}"))
}

fn call(server: &ArcServer, name: &str, arguments: Value) -> CallToolResult {
    server.call(name, arguments.as_object().cloned())
}

/// Asserts a tool's result is a successful structured object and returns it: its
/// `structuredContent` is present and is exactly the parsed text content.
fn structured(result: &CallToolResult, tool: &str) -> Value {
    assert_eq!(result.is_error, Some(false), "`{tool}` succeeds");
    let value = value_of(result);
    assert!(
        value.is_object(),
        "`{tool}` answers with an object: {value}"
    );
    assert_eq!(
        result.structured_content.as_ref(),
        Some(&value),
        "`{tool}`: structured content is the text content's JSON"
    );
    value
}

#[test]
fn list_on_an_empty_store_is_text_only_and_is_the_list_json_array() {
    let scratch = fixture("list-empty");
    let store = Store::at(scratch.repo());
    let server = ArcServer::new(store.clone());

    let result = call(&server, "list", json!({}));
    assert_eq!(
        result.is_error,
        Some(false),
        "`list` on an empty store succeeds"
    );
    assert_eq!(
        result.structured_content,
        None,
        "an array is not a structured result: {}",
        text_of(&result)
    );
    assert_eq!(value_of(&result), json!([]), "an empty store lists nothing");
    assert_eq!(
        value_of(&result),
        list_json(&store, None, false).expect("`list --json` reads the store"),
        "the text is the array `list --json` prints"
    );
}

#[test]
fn list_with_a_plan_is_text_only_and_is_the_list_json_array() {
    let scratch = fixture("list-plan");
    let store = Store::at(scratch.repo());
    let plan = store
        .add_plan("plan", "Plan", "the request")
        .expect("a plan");
    store
        .add_task(
            &plan,
            "work",
            "Task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief",
        )
        .expect("a task");
    let server = ArcServer::new(store.clone());

    let result = call(&server, "list", json!({}));
    assert_eq!(result.is_error, Some(false));
    assert_eq!(
        result.structured_content,
        None,
        "`list` keeps the array out of `structuredContent`: {}",
        text_of(&result)
    );
    let value = value_of(&result);
    assert_eq!(
        value,
        list_json(&store, None, false).expect("`list --json` reads the store"),
        "the text is the array `list --json` prints"
    );
    assert_eq!(value[0]["id"], plan.as_str());
    assert_eq!(value[0]["tasks"][0]["id"], "TK0001-work");
}

#[test]
fn show_is_structured_and_structured_content_equals_the_parsed_text() {
    let scratch = fixture("show");
    let store = Store::at(scratch.repo());
    let plan = store
        .add_plan("plan", "Plan", "the request")
        .expect("a plan");
    let task = store
        .add_task(
            &plan,
            "work",
            "Task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief",
        )
        .expect("a task");
    let server = ArcServer::new(store.clone());

    for id in [&plan, &task] {
        let result = call(&server, "show", json!({ "id": id }));
        assert_eq!(result.is_error, Some(false), "`show {id}` succeeds");
        let structured = result
            .structured_content
            .as_ref()
            .unwrap_or_else(|| panic!("`show {id}` is an object and is structured"));
        assert!(structured.is_object(), "`show {id}`: {structured}");
        assert_eq!(
            structured,
            &value_of(&result),
            "`show {id}`: structured content is the text content's JSON"
        );
        assert_eq!(
            structured,
            &show_json(&store, id).expect("`show --json` reads the store"),
            "`show {id}`: the same document `show --json` prints"
        );
    }
}

#[test]
fn every_other_tool_answers_with_an_object() {
    let scratch = fixture("object-tools");
    let store = Store::at(scratch.repo());
    let server = ArcServer::new(store.clone());

    let plan = structured(
        &call(
            &server,
            "add_plan",
            json!({"slug": "plan", "title": "Plan", "request": "the request"}),
        ),
        "add_plan",
    );
    let plan = plan["id"].as_str().expect("a plan id").to_owned();
    let task = structured(
        &call(
            &server,
            "add_task",
            json!({"plan": plan, "slug": "work", "title": "Task", "brief": "the brief"}),
        ),
        "add_task",
    );
    let task = task["id"].as_str().expect("a task id").to_owned();

    structured(
        &call(
            &server,
            "update_plan",
            json!({"plan": plan, "title": "Renamed"}),
        ),
        "update_plan",
    );
    structured(
        &call(
            &server,
            "update_task",
            json!({"task": task, "title": "Renamed"}),
        ),
        "update_task",
    );
    store
        .change_task(&task, |task| {
            task.state = radian_arc::store::model::TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            Ok(())
        })
        .expect("the task is put in review");
    structured(
        &call(
            &server,
            "add_rework",
            json!({"task": task, "brief": "again"}),
        ),
        "add_rework",
    );
}

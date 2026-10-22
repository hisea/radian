//! `radian arc mcp` through the real binary and a real MCP client on stdio: the six tools,
//! the plan and task rules they keep, and the reasons they refuse (ADR 0048 decisions 3, 6
//! and 11).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::store::Store;
use radian_arc::store::model::TaskState;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-arc-mcp-{label}-{}-{n}", std::process::id()));
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

/// A repository with a bare `origin`, set up for arc. The layout comes from the store's own
/// `init`, so the test does not hand-write anything arc writes.
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
    fs::write(scratch.repo().join("README.md"), "hello\n").expect("the file is writable");
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

/// The MCP client: no capabilities, so the server can never wait on it.
#[derive(Clone)]
struct TestClient;

impl ClientHandler for TestClient {}

/// One `radian arc mcp` process and the client connected to it.
struct Session {
    service: RunningService<RoleClient, TestClient>,
    child: tokio::process::Child,
}

impl Session {
    async fn open(scratch: &Scratch) -> Self {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_radian"));
        command
            .current_dir(scratch.repo())
            .env("RADIAN_HOME", scratch.path("home"))
            .args(["arc", "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().expect("`radian arc mcp` starts");
        let stdin = child.stdin.take().expect("the server's stdin is piped");
        let stdout = child.stdout.take().expect("the server's stdout is piped");
        let service = TestClient
            .serve((stdout, stdin))
            .await
            .expect("the MCP handshake succeeds");
        Self { service, child }
    }

    /// The tool names the server lists, sorted.
    async fn tools(&self) -> Vec<String> {
        let tools = self
            .service
            .peer()
            .list_all_tools()
            .await
            .expect("tools/list succeeds");
        let mut names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        names.sort();
        names
    }

    async fn call(&self, name: &str, arguments: Value) -> CallToolResult {
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = arguments.as_object().cloned();
        self.service
            .call_tool(params)
            .await
            .unwrap_or_else(|error| panic!("`{name}` did not answer: {error}"))
    }

    /// Calls a tool that must succeed and returns its JSON result.
    async fn ok(&self, name: &str, arguments: Value) -> Value {
        let result = self.call(name, arguments).await;
        let text = result_text(&result);
        assert_ne!(result.is_error, Some(true), "`{name}` failed: {text}");
        serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|error| panic!("`{name}` did not answer JSON ({error}): {text}"))
    }

    /// Calls a tool that must fail and returns the reason.
    async fn error(&self, name: &str, arguments: Value) -> String {
        let result = self.call(name, arguments).await;
        assert_eq!(
            result.is_error,
            Some(true),
            "`{name}` must be a tool error, not {:?}",
            result_text(&result)
        );
        result_text(&result)
    }

    async fn close(mut self) {
        let _ = self.service.cancel().await;
        let _ = self.child.wait().await;
    }
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Adds a plan with one task through the tools and returns both ids.
async fn plan_with_task(session: &Session, slug: &str) -> (String, String) {
    let plan = session
        .ok(
            "add_plan",
            json!({"slug": slug, "title": "A plan", "request": "the request"}),
        )
        .await;
    let plan = plan["id"].as_str().expect("a plan id").to_owned();
    let task = session
        .ok(
            "add_task",
            json!({
                "plan": plan,
                "slug": "work",
                "title": "do it",
                "brief": "the brief",
            }),
        )
        .await;
    let task = task["id"].as_str().expect("a task id").to_owned();
    (plan, task)
}

fn task_path(scratch: &Scratch, task: &str) -> PathBuf {
    scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{task}.md"))
}

fn set_state(scratch: &Scratch, task: &str, state: TaskState, phase: &str) {
    Store::at(scratch.repo())
        .change_task(task, |task| {
            task.state = state;
            task.phase = phase.to_owned();
            Ok(())
        })
        .expect("the test moves the task");
}

#[tokio::test]
async fn tools_list_returns_exactly_the_six_lead_tools() {
    let scratch = fixture("mcp-tools");
    let session = Session::open(&scratch).await;

    let names = session.tools().await;
    let mut expected: Vec<String> = radian_arc::mcp::names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    expected.sort();
    assert_eq!(names, expected, "the listed tools are the code's list");
    assert_eq!(
        names,
        vec![
            "add_plan".to_owned(),
            "add_rework".to_owned(),
            "add_task".to_owned(),
            "list".to_owned(),
            "show".to_owned(),
            "update_plan".to_owned(),
        ],
        "the full set: no owner action is a tool"
    );
    for forbidden in ["confirm", "start", "merge", "reject", "auto", "approve"] {
        assert!(
            !names.iter().any(|name| name.contains(forbidden)),
            "`{forbidden}` must not be a tool: {names:?}"
        );
    }
    session.close().await;
}

#[tokio::test]
async fn add_plan_add_task_and_list_work() {
    let scratch = fixture("mcp-add");
    let session = Session::open(&scratch).await;

    let plan = session
        .ok(
            "add_plan",
            json!({"slug": "first", "title": "A plan", "request": "the request"}),
        )
        .await;
    assert_eq!(plan["id"], "0001-first");

    let task = session
        .ok(
            "add_task",
            json!({
                "plan": plan["id"],
                "slug": "work",
                "title": "do it",
                "brief": "the brief",
                "allow_noop": true,
                "extra_stages": ["mutation"],
            }),
        )
        .await;
    assert_eq!(task["id"], "0001-work");

    // The optional arguments may be left out.
    let second = session
        .ok(
            "add_task",
            json!({
                "plan": plan["id"],
                "slug": "more",
                "title": "and more",
                "brief": "another brief",
            }),
        )
        .await;
    assert_eq!(second["id"], "0002-more");

    let list = session.ok("list", json!({})).await;
    assert_eq!(list[0]["id"], plan["id"]);
    assert_eq!(list[0]["status"], "draft");
    assert_eq!(list[0]["tasks"][0]["id"], task["id"]);
    assert_eq!(list[0]["tasks"][0]["state"], "todo");
    assert_eq!(list[0]["tasks"][0]["phase"], "ready");
    assert_eq!(list[0]["tasks"][1]["id"], second["id"]);

    let one = session.ok("list", json!({"plan": plan["id"]})).await;
    assert_eq!(one.as_array().expect("an array").len(), 1);

    let shown = session.ok("show", json!({"id": task["id"]})).await;
    assert_eq!(shown["plan"], plan["id"]);
    assert_eq!(shown["allow_noop"], true);
    assert_eq!(shown["extra_stages"][0], "mutation");
    assert!(
        shown["body"]
            .as_str()
            .expect("a body")
            .contains("the brief"),
        "{shown}"
    );

    // A missing argument is a tool error that names it.
    let error = session
        .error("add_task", json!({"plan": plan["id"], "slug": "x"}))
        .await;
    assert!(error.contains("title"), "{error}");
    let error = session.error("show", json!({"id": "0099-missing"})).await;
    assert!(error.contains("no plan or task"), "{error}");
    session.close().await;
}

#[tokio::test]
async fn update_plan_on_a_confirmed_plan_appends_a_record_and_refuses_the_request() {
    let scratch = fixture("mcp-update");
    let session = Session::open(&scratch).await;
    let (plan, task) = plan_with_task(&session, "update").await;

    // A draft plan takes a title and a request, but no change record.
    let draft = session
        .ok(
            "update_plan",
            json!({"plan": plan, "title": "A better title", "request": "a better request"}),
        )
        .await;
    assert_eq!(draft["title"], "A better title");
    assert_eq!(draft["status"], "draft");
    assert!(
        draft["body"]
            .as_str()
            .expect("a body")
            .contains("a better request"),
        "{draft}"
    );
    let refused = session
        .error(
            "update_plan",
            json!({"plan": plan, "change_record": "too early"}),
        )
        .await;
    assert!(refused.contains("draft"), "{refused}");

    radian_arc::store::plan_ops::confirm(&Store::at(scratch.repo()), &plan).expect("confirmed");

    // A confirmed plan's title and request text are refused...
    for arguments in [
        json!({"plan": plan, "request": "a rewritten request"}),
        json!({"plan": plan, "title": "A rewritten title"}),
    ] {
        let refused = session.error("update_plan", arguments).await;
        assert!(refused.contains("confirmed"), "{refused}");
        assert!(refused.contains("change record"), "{refused}");
    }

    // ...and a change record is appended, returning the plan to draft with auto off.
    let updated = session
        .ok(
            "update_plan",
            json!({"plan": plan, "change_record": "the scope grew"}),
        )
        .await;
    assert_eq!(updated["status"], "draft");
    assert_eq!(updated["auto"], "off");
    let body = updated["body"].as_str().expect("a body");
    assert!(body.contains("the scope grew"), "{body}");
    assert!(
        body.contains("a better request"),
        "the request stays: {body}"
    );
    assert!(!body.contains("a rewritten request"), "{body}");
    assert!(
        body.find("a better request").unwrap() < body.find("the scope grew").unwrap(),
        "the record goes under the change-records heading: {body}"
    );
    // The tasks keep their state, and the file on disk holds the record with its time.
    assert_eq!(updated["tasks"][0]["id"], task);
    assert_eq!(updated["tasks"][0]["state"], "todo");
    let text = fs::read_to_string(
        scratch
            .repo()
            .join(".radian/arc/plans")
            .join(format!("{plan}.md")),
    )
    .expect("the plan file is readable");
    assert!(text.contains("## Change records"), "{text}");
    let time_line = text
        .lines()
        .find(|line| line.starts_with("### 2"))
        .unwrap_or_else(|| panic!("no dated record in {text}"));
    let time = time_line.trim_start_matches("### ");
    assert!(
        radian_arc::store::model::valid_utc(time),
        "`{time}` is not an absolute UTC time"
    );
    session.close().await;
}

#[tokio::test]
async fn add_rework_from_a_review_task_moves_it_to_ready() {
    let scratch = fixture("mcp-rework");
    let session = Session::open(&scratch).await;
    let (_plan, task) = plan_with_task(&session, "rework").await;

    // A task in `todo` is refused: a rework brief continues a task in review.
    let refused = session
        .error("add_rework", json!({"task": task, "brief": "again"}))
        .await;
    assert!(refused.contains("review"), "{refused}");

    set_state(&scratch, &task, TaskState::Review, "awaiting_human");
    let reworked = session
        .ok(
            "add_rework",
            json!({"task": task, "brief": "make it faster"}),
        )
        .await;
    assert_eq!(reworked["state"], "todo");
    assert_eq!(reworked["phase"], "ready");
    let body = reworked["body"].as_str().expect("a body");
    assert!(body.contains("the brief"), "the first brief stays: {body}");
    assert!(body.contains("## Rework 1"), "{body}");
    assert!(body.contains("make it faster"), "{body}");

    // Once back in `todo` it is refused again.
    let refused = session
        .error("add_rework", json!({"task": task, "brief": "again"}))
        .await;
    assert!(refused.contains("review"), "{refused}");

    // The next rework is numbered by the task's attempt.
    Store::at(scratch.repo())
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 2;
            Ok(())
        })
        .expect("the attempt moves");
    let reworked = session
        .ok("add_rework", json!({"task": task, "brief": "third time"}))
        .await;
    let body = reworked["body"].as_str().expect("a body");
    assert!(body.contains("## Rework 3"), "{body}");
    session.close().await;
}

#[tokio::test]
async fn a_hand_edited_task_makes_add_rework_fail_with_changed_outside_arc() {
    let scratch = fixture("mcp-outside");
    let session = Session::open(&scratch).await;
    let (_plan, task) = plan_with_task(&session, "outside").await;
    set_state(&scratch, &task, TaskState::Review, "awaiting_human");

    let path = task_path(&scratch, &task);
    let text = fs::read_to_string(&path).expect("the task file is readable");
    fs::write(&path, text.replace("the brief", "an edited brief")).expect("the edit is writable");

    let refused = session
        .error("add_rework", json!({"task": task, "brief": "a rework"}))
        .await;
    assert!(refused.contains("changed outside arc"), "{refused}");
    // The edit is still there: a refused call writes nothing.
    let text = fs::read_to_string(&path).expect("the task file is readable");
    assert!(text.contains("an edited brief"), "{text}");
    assert!(!text.contains("## Rework"), "{text}");
    session.close().await;
}

//! `radian arc mcp` through the real binary and a real MCP client on stdio: the seven tools,
//! the plan and task rules they keep, the reasons they refuse, and the shape of a successful
//! result (ADR 0048 decisions 3, 6 and 11).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use radian_arc::store::Store;
use radian_arc::store::model::TaskState;
use rmcp::model::{CallToolRequestParams, CallToolResult, CustomNotification};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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

/// The MCP client: no capabilities, so the server can never wait on it. It records every
/// custom notification, which is how the channel is observed.
#[derive(Clone)]
struct TestClient {
    events: UnboundedSender<CustomNotification>,
}

impl ClientHandler for TestClient {
    fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let _ = self.events.send(notification);
        std::future::ready(())
    }
}

/// One `radian arc mcp` process and the client connected to it.
struct Session {
    service: RunningService<RoleClient, TestClient>,
    child: tokio::process::Child,
    events: UnboundedReceiver<CustomNotification>,
}

impl Session {
    /// The plain server: no `--claude-experimental`.
    async fn open(scratch: &Scratch) -> Self {
        Self::open_flagged(scratch, false).await
    }

    /// The server, with the channel when `channel` is set.
    async fn open_flagged(scratch: &Scratch, channel: bool) -> Self {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_radian"));
        command
            .current_dir(scratch.repo())
            .env("RADIAN_HOME", scratch.path("home"))
            .args(["arc", "mcp"]);
        if channel {
            command.arg("--claude-experimental");
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().expect("`radian arc mcp` starts");
        let stdin = child.stdin.take().expect("the server's stdin is piped");
        let stdout = child.stdout.take().expect("the server's stdout is piped");
        let (events, receiver) = unbounded_channel();
        let service = TestClient { events }
            .serve((stdout, stdin))
            .await
            .expect("the MCP handshake succeeds");
        Self {
            service,
            child,
            events: receiver,
        }
    }

    /// The server capabilities the handshake agreed on.
    fn capabilities(&self) -> rmcp::model::ServerCapabilities {
        self.service
            .peer()
            .peer_info()
            .expect("the handshake records the server's capabilities")
            .capabilities
            .clone()
    }

    /// The wait for one channel event: up to 15 seconds, never a fixed sleep.
    async fn next_event(&mut self) -> CustomNotification {
        tokio::time::timeout(Duration::from_secs(15), self.events.recv())
            .await
            .expect("a channel event arrives within 15 seconds")
            .expect("the event channel stays open")
    }

    /// Waits the given time and asserts that no event arrived. A negative check has to cover
    /// at least one poll round; two rounds keep it stable on a loaded machine.
    async fn no_event_for(&mut self, wait: Duration) {
        match tokio::time::timeout(wait, self.events.recv()).await {
            Err(_) => {}
            Ok(Some(notification)) => panic!(
                "expected no channel event, got {} {:?}",
                notification.method, notification.params
            ),
            Ok(None) => panic!("the event channel closed"),
        }
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

/// The text content parsed as JSON: the document the tool answers with.
fn json_of(result: &CallToolResult) -> Value {
    let text = result_text(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the text content is not JSON ({error}): {text}"))
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

/// Asserts one channel event for `task` and returns its content.
fn expect_event(
    notification: &CustomNotification,
    task: &str,
    plan: &str,
    state: &str,
    phase: &str,
    previous: &str,
) -> String {
    assert_eq!(
        notification.method,
        radian_arc::mcp::CHANNEL_METHOD,
        "the event method is the channel's"
    );
    let params = notification.params.as_ref().expect("the event has params");
    let content = params["content"]
        .as_str()
        .expect("the event content is a string")
        .to_owned();
    let meta = params["meta"].as_object().expect("the meta is an object");
    let mut keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["phase", "plan", "previous", "state", "task"],
        "the meta has exactly the five channel keys: {meta:?}"
    );
    for (key, value) in meta {
        assert!(value.is_string(), "meta `{key}` is a string: {value}");
    }
    assert_eq!(meta["task"].as_str(), Some(task), "{meta:?}");
    assert_eq!(meta["plan"].as_str(), Some(plan), "{meta:?}");
    assert_eq!(meta["state"].as_str(), Some(state), "{meta:?}");
    assert_eq!(meta["phase"].as_str(), Some(phase), "{meta:?}");
    assert_eq!(meta["previous"].as_str(), Some(previous), "{meta:?}");
    let first = content.lines().next().unwrap_or_default();
    assert_eq!(
        first,
        format!("{task} {previous} -> {state}.{phase}"),
        "the content's first line: {content:?}"
    );
    content
}

/// Asserts one channel plan event and returns its content. A plan event carries `plan`,
/// `field`, `previous` and `value`, and no `task` key, so a reader can tell it from a task
/// event.
fn expect_plan_event(
    notification: &CustomNotification,
    plan: &str,
    field: &str,
    previous: &str,
    value: &str,
) -> String {
    assert_eq!(
        notification.method,
        radian_arc::mcp::CHANNEL_METHOD,
        "the event method is the channel's"
    );
    let params = notification.params.as_ref().expect("the event has params");
    let content = params["content"]
        .as_str()
        .expect("the event content is a string")
        .to_owned();
    let meta = params["meta"].as_object().expect("the meta is an object");
    let mut keys: Vec<&str> = meta.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["field", "plan", "previous", "value"],
        "the meta has exactly the four plan keys and no task: {meta:?}"
    );
    for (key, value) in meta {
        assert!(value.is_string(), "meta `{key}` is a string: {value}");
    }
    assert_eq!(meta["plan"].as_str(), Some(plan), "{meta:?}");
    assert_eq!(meta["field"].as_str(), Some(field), "{meta:?}");
    assert_eq!(meta["previous"].as_str(), Some(previous), "{meta:?}");
    assert_eq!(meta["value"].as_str(), Some(value), "{meta:?}");
    let first = content.lines().next().unwrap_or_default();
    assert_eq!(
        first,
        format!("{plan} {field} {previous} -> {value}"),
        "the content's first line: {content:?}"
    );
    content
}

/// One raw JSON-RPC line to a fresh `radian arc mcp` process, and the one line it answers.
/// A raw probe is how the bootstrap `server/discover` path is tested: the typed client does
/// not send it.
fn raw_request(scratch: &Scratch, channel: bool, request: Value) -> Value {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_radian"));
    command
        .current_dir(scratch.repo())
        .env("RADIAN_HOME", scratch.path("home"))
        .args(["arc", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if channel {
        command.arg("--claude-experimental");
    }
    let mut child = command.spawn().expect("`radian arc mcp` starts");
    let mut stdin = child.stdin.take().expect("the server's stdin is piped");
    writeln!(stdin, "{request}").expect("the request is written");
    stdin.flush().expect("the request reaches the server");
    let stdout = child.stdout.take().expect("the server's stdout is piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("the server answers one line");
    drop(stdin);
    let _ = child.wait();
    serde_json::from_str(&line)
        .unwrap_or_else(|error| panic!("the answer is not JSON ({error}): {line}"))
}

/// A `server/discover` request with the 2026-07-28 request metadata the draft schema needs.
fn discover_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
}

/// An `initialize` request that asks for the revision a channel cannot use.
fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {
            "protocolVersion": "2026-07-28",
            "capabilities": {},
            "clientInfo": {"name": "arc-test", "version": "0.0.0"}
        }
    })
}

#[tokio::test]
async fn tools_list_returns_exactly_the_seven_lead_tools() {
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
            "update_task".to_owned(),
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
    assert_eq!(plan["id"], "PL0001-first");

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
    assert_eq!(task["id"], "TK0001-work");

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
    assert_eq!(second["id"], "TK0002-more");

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
    let error = session.error("show", json!({"id": "TK0099-missing"})).await;
    assert!(error.contains("no plan or task"), "{error}");
    session.close().await;
}

/// ADR 0048 decision 11: `structuredContent` is a JSON object or absent. `list` answers with
/// the array `radian arc list --json` prints, so the result a client receives carries the text
/// only; `show` answers with an object and carries it both ways. This goes through the real
/// `radian arc mcp` process and a real client, so `call_tool`, the response conversion and the
/// wire are covered, not only the handler's own helper.
#[tokio::test]
async fn list_is_text_only_and_show_is_structured() {
    let scratch = fixture("mcp-result-shape");
    let session = Session::open(&scratch).await;

    // An empty store: the array `[]` in the text, nothing in `structuredContent`.
    let empty = session.call("list", json!({})).await;
    assert_eq!(
        empty.is_error,
        Some(false),
        "`list` succeeds on an empty store"
    );
    assert_eq!(
        empty.structured_content,
        None,
        "an array is not a structured result: {}",
        result_text(&empty)
    );
    assert_eq!(json_of(&empty), json!([]), "an empty store lists nothing");

    // A plan with a task: still the array, and the same one `list --json` prints.
    let (plan, task) = plan_with_task(&session, "shape").await;
    let listed = session.call("list", json!({})).await;
    assert_eq!(listed.is_error, Some(false), "`list` succeeds");
    assert_eq!(
        listed.structured_content,
        None,
        "`list` keeps the array out of `structuredContent`: {}",
        result_text(&listed)
    );
    let value = json_of(&listed);
    assert_eq!(
        value,
        radian_arc::store::cli::list_json(&Store::at(scratch.repo()), None, false)
            .expect("`list --json` reads the store"),
        "the text is the array `list --json` prints"
    );
    assert_eq!(value[0]["id"], plan.as_str());
    assert_eq!(value[0]["tasks"][0]["id"], task.as_str());

    // An object is structured: `show` sends its JSON as text and as `structuredContent`.
    for id in [&plan, &task] {
        let shown = session.call("show", json!({ "id": id })).await;
        assert_eq!(shown.is_error, Some(false), "`show {id}` succeeds");
        let structured = shown
            .structured_content
            .as_ref()
            .unwrap_or_else(|| panic!("`show {id}` is an object and is structured"));
        assert!(structured.is_object(), "`show {id}`: {structured}");
        assert_eq!(
            structured,
            &json_of(&shown),
            "`show {id}`: structured content is the text content's JSON"
        );
    }

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
async fn add_task_on_a_confirmed_plan_is_proposed_and_the_plan_is_unchanged() {
    let scratch = fixture("mcp-proposed");
    let session = Session::open(&scratch).await;
    let (plan, task) = plan_with_task(&session, "confirmed-plan").await;
    let store = Store::at(scratch.repo());
    radian_arc::store::plan_ops::confirm(&store, &plan).expect("confirmed");
    radian_arc::store::plan_ops::set_auto(&store, &plan, "run").expect("auto run");

    let proposed = session
        .ok(
            "add_task",
            json!({
                "plan": plan,
                "slug": "follow",
                "title": "Follow up",
                "brief": "the follow-up",
            }),
        )
        .await;
    let id = proposed["id"].as_str().expect("a task id").to_owned();
    assert_ne!(id, task, "a new task, not the draft one");

    let shown = session.ok("show", json!({"id": id})).await;
    assert_eq!(shown["state"], "todo");
    assert_eq!(shown["phase"], "proposed");
    assert_eq!(shown["plan"], plan);

    // The plan keeps its status and auto, and the task is on it.
    let listed = session.ok("list", json!({"plan": plan})).await;
    assert_eq!(listed[0]["status"], "confirmed");
    assert_eq!(listed[0]["auto"], "run");
    assert_eq!(listed[0]["tasks"][0]["id"], task);
    assert_eq!(listed[0]["tasks"][0]["phase"], "ready");
    assert_eq!(listed[0]["tasks"][1]["id"], id.as_str());
    assert_eq!(listed[0]["tasks"][1]["phase"], "proposed");

    let text = fs::read_to_string(task_path(&scratch, &id)).expect("the task file is readable");
    assert!(text.contains("phase: proposed"), "{text}");
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

#[tokio::test]
async fn without_the_flag_the_server_is_todays_server() {
    let scratch = fixture("mcp-plain-server");
    let mut session = Session::open(&scratch).await;

    // Today's capabilities and today's version list.
    assert!(
        session.capabilities().experimental.is_none(),
        "without the flag no experimental capability is declared"
    );
    let discover = raw_request(&scratch, false, discover_request());
    assert!(
        discover.get("error").is_none_or(Value::is_null),
        "server/discover still succeeds: {discover}"
    );
    let versions = discover["result"]["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("a supportedVersions list: {discover}"));
    assert!(
        versions.iter().any(|version| version == "2026-07-28"),
        "today's supported versions still include 2026-07-28: {discover}"
    );

    // A tracked transition sends nothing.
    let (_plan, task) = plan_with_task(&session, "plain").await;
    set_state(&scratch, &task, TaskState::Review, "awaiting_human");
    session.no_event_for(Duration::from_secs(11)).await;
    session.close().await;
}

#[tokio::test]
async fn the_flag_declares_the_channel_and_refuses_discover() {
    let scratch = fixture("mcp-channel-capability");
    let session = Session::open_flagged(&scratch, true).await;

    let capabilities = session.capabilities();
    let experimental = capabilities
        .experimental
        .expect("the flagged server declares an experimental capability map");
    let channel = experimental
        .get(radian_arc::mcp::CHANNEL_CAPABILITY)
        .expect("claude/channel is declared");
    assert!(
        channel.is_empty(),
        "the capability's value is an empty object"
    );
    assert!(
        !experimental.contains_key(radian_arc::mcp::CHANNEL_PERMISSION_CAPABILITY),
        "the permission relay is not declared: {experimental:?}"
    );

    // The discover probe is refused: the server supports no revision that carries channels.
    let discover = raw_request(&scratch, true, discover_request());
    let error = discover
        .get("error")
        .filter(|error| !error.is_null())
        .unwrap_or_else(|| panic!("server/discover is refused: {discover}"));
    assert_eq!(error["code"], -32022, "{discover}");
    assert_eq!(
        error["message"], "Unsupported protocol version",
        "{discover}"
    );
    let supported = error["data"]["supported"]
        .as_array()
        .unwrap_or_else(|| panic!("the error names the supported versions: {discover}"));
    assert!(
        !supported.iter().any(|version| version == "2026-07-28"),
        "2026-07-28 is not supported: {discover}"
    );
    assert!(
        supported.iter().any(|version| version == "2025-11-25"),
        "2025-11-25 is the ceiling: {discover}"
    );

    // An initialize that asks for 2026-07-28 is answered with 2025-11-25.
    let initialize = raw_request(&scratch, true, initialize_request());
    assert_eq!(
        initialize["result"]["protocolVersion"], "2025-11-25",
        "{initialize}"
    );
    assert_eq!(
        initialize["result"]["capabilities"]["experimental"]["claude/channel"],
        json!({}),
        "{initialize}"
    );

    session.close().await;
}

#[tokio::test]
async fn the_channel_reports_the_tracked_transitions_and_stays_silent_for_the_rest() {
    let scratch = fixture("mcp-channel-events");

    // The plan and its two tasks come from the tools, so the baseline is a real backlog.
    let setup = Session::open(&scratch).await;
    let (plan, first) = plan_with_task(&setup, "channel").await;
    let second = setup
        .ok(
            "add_task",
            json!({
                "plan": plan,
                "slug": "second",
                "title": "the second task",
                "brief": "the second brief",
            }),
        )
        .await["id"]
        .as_str()
        .expect("a task id")
        .to_owned();
    setup.close().await;

    // The first task is already `review.awaiting_human` when the channel opens: the baseline
    // must stay silent for it. Its attempt number is what a reject note hangs off.
    let store = Store::at(scratch.repo());
    store
        .change_task(&first, |task| {
            task.state = TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            task.attempt = 1;
            Ok(())
        })
        .expect("the task is put in review");

    let mut session = Session::open_flagged(&scratch, true).await;
    session.no_event_for(Duration::from_secs(11)).await;

    // `review.awaiting_human` -> `review.rework_pending` with a recorded note: the note is
    // the event's second content line.
    radian_arc::review::reject(&store, &first, "needs a faster loop")
        .expect("the task is rejected");
    let event = session.next_event().await;
    let content = expect_event(
        &event,
        &first,
        &plan,
        "review",
        "rework_pending",
        "review.awaiting_human",
    );
    assert_eq!(
        content,
        format!("{first} review.awaiting_human -> review.rework_pending\nneeds a faster loop")
    );

    // `review.rework_pending` -> `done.merged`: no `rework_reason` any more.
    set_state(&scratch, &first, TaskState::Done, "merged");
    let event = session.next_event().await;
    let content = expect_event(
        &event,
        &first,
        &plan,
        "done",
        "merged",
        "review.rework_pending",
    );
    assert!(!content.contains('\n'), "one line, no reason: {content:?}");

    // `doing.running` is not a sent phase: the move costs the lead nothing.
    set_state(&scratch, &second, TaskState::Doing, "running");
    session.no_event_for(Duration::from_secs(11)).await;

    // The phases on either side of that quiet move are sent, with the actual previous phase.
    set_state(&scratch, &second, TaskState::Review, "awaiting_human");
    let event = session.next_event().await;
    expect_event(
        &event,
        &second,
        &plan,
        "review",
        "awaiting_human",
        "doing.running",
    );

    set_state(&scratch, &second, TaskState::Failed, "cancelled");
    let event = session.next_event().await;
    expect_event(
        &event,
        &second,
        &plan,
        "failed",
        "cancelled",
        "review.awaiting_human",
    );

    // A task added to a confirmed plan is a transition from nothing. The confirm itself is
    // the plan's `status` event, sent before the task is added.
    radian_arc::store::plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
    let event = session.next_event().await;
    let content = expect_plan_event(&event, &plan, "status", "draft", "confirmed");
    assert_eq!(content, format!("{plan} status draft -> confirmed"));

    let added = session
        .ok(
            "add_task",
            json!({
                "plan": plan,
                "slug": "follow",
                "title": "the follow-up",
                "brief": "the follow-up brief",
            }),
        )
        .await;
    let added = added["id"].as_str().expect("a task id").to_owned();
    let event = session.next_event().await;
    expect_event(&event, &added, &plan, "todo", "proposed", "new");

    session.close().await;
}

#[tokio::test]
async fn a_task_file_the_poller_cannot_parse_sends_nothing_and_the_next_transition_arrives() {
    let scratch = fixture("mcp-channel-bad-read");

    let setup = Session::open(&scratch).await;
    let (plan, task) = plan_with_task(&setup, "bad-read").await;
    setup.close().await;

    let mut session = Session::open_flagged(&scratch, true).await;
    session.no_event_for(Duration::from_secs(11)).await;

    set_state(&scratch, &task, TaskState::Review, "awaiting_human");
    let event = session.next_event().await;
    expect_event(
        &event,
        &task,
        &plan,
        "review",
        "awaiting_human",
        "todo.ready",
    );

    // A task file that does not parse fails the whole read: nothing is sent that round.
    let path = task_path(&scratch, &task);
    let original = fs::read_to_string(&path).expect("the task file is readable");
    fs::write(&path, "this is not a task file\n").expect("the task file is writable");
    session.no_event_for(Duration::from_secs(11)).await;

    // The next round reads again, and a valid transition after it still arrives.
    fs::write(&path, &original).expect("the task file is restored");
    set_state(&scratch, &task, TaskState::Done, "merged");
    let event = session.next_event().await;
    expect_event(
        &event,
        &task,
        &plan,
        "done",
        "merged",
        "review.awaiting_human",
    );

    session.close().await;
}

#[tokio::test]
async fn the_channel_reports_the_owner_confirm_and_the_rework_into_ready() {
    let scratch = fixture("mcp-channel-owner-task");

    // The first task is `todo.ready` and the second is `todo.proposed`: the plan is confirmed
    // before the second task is added, and both are in place when the channel opens.
    let setup = Session::open(&scratch).await;
    let (plan, first) = plan_with_task(&setup, "owner-task").await;
    let store = Store::at(scratch.repo());
    radian_arc::store::plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
    let second = setup
        .ok(
            "add_task",
            json!({
                "plan": plan,
                "slug": "second",
                "title": "the proposed task",
                "brief": "the proposed brief",
            }),
        )
        .await["id"]
        .as_str()
        .expect("a task id")
        .to_owned();
    setup.close().await;

    let mut session = Session::open_flagged(&scratch, true).await;
    session.no_event_for(Duration::from_secs(11)).await;

    // A title edit changes no phase and sends nothing.
    session
        .ok(
            "update_task",
            json!({"task": first, "title": "a better title"}),
        )
        .await;
    session.no_event_for(Duration::from_secs(11)).await;

    // The owner's confirm of a proposed task: a transition into a phase that does not send by
    // itself, with the same shape as every task event.
    radian_arc::store::plan_ops::confirm(&store, &second).expect("the task is confirmed");
    let event = session.next_event().await;
    let content = expect_event(&event, &second, &plan, "todo", "ready", "todo.proposed");
    assert_eq!(content, format!("{second} todo.proposed -> todo.ready"));

    // A task that leaves review for work is sent: the owner's reject moves to
    // `review.rework_pending` (and its note), and the lead's `add_rework` is the same kind of
    // move into `todo.ready`.
    set_state(&scratch, &first, TaskState::Review, "rework_pending");
    let event = session.next_event().await;
    expect_event(
        &event,
        &first,
        &plan,
        "review",
        "rework_pending",
        "todo.ready",
    );

    radian_arc::store::plan_ops::add_rework(&store, &first, "a rework")
        .expect("the lead adds a rework");
    let event = session.next_event().await;
    let content = expect_event(
        &event,
        &first,
        &plan,
        "todo",
        "ready",
        "review.rework_pending",
    );
    assert_eq!(
        content,
        format!("{first} review.rework_pending -> todo.ready")
    );

    // A conflict rework arc writes itself is not an owner action: its heading says so and the
    // move sends nothing.
    set_state(&scratch, &first, TaskState::Review, "rework_pending");
    let event = session.next_event().await;
    expect_event(
        &event,
        &first,
        &plan,
        "review",
        "rework_pending",
        "todo.ready",
    );
    radian_arc::store::plan_ops::add_rework_with(
        &store,
        &first,
        "the conflict with main",
        radian_arc::store::plan_ops::CONFLICT_REWORK_SUFFIX,
    )
    .expect("arc writes the conflict rework");
    session.no_event_for(Duration::from_secs(11)).await;

    session.close().await;
}

#[tokio::test]
async fn the_channel_reports_the_plan_confirm_and_pause_and_stays_silent_for_the_rest() {
    let scratch = fixture("mcp-channel-plan");
    let setup = Session::open(&scratch).await;
    let (plan, task) = plan_with_task(&setup, "owner-plan").await;
    setup.close().await;
    let store = Store::at(scratch.repo());

    let mut session = Session::open_flagged(&scratch, true).await;
    session.no_event_for(Duration::from_secs(11)).await;

    // A title edit and a priority change are not sent.
    radian_arc::store::plan_ops::update_plan(&store, &plan, Some("Another plan"), None, None)
        .expect("the draft title changes");
    radian_arc::store::plan_ops::set_priority(&store, &plan, "P1").expect("the priority changes");
    session.no_event_for(Duration::from_secs(11)).await;

    // The owner confirms the plan: the plan's `status` event, with no `task` key.
    radian_arc::store::plan_ops::confirm(&store, &plan).expect("the plan is confirmed");
    let event = session.next_event().await;
    let content = expect_plan_event(&event, &plan, "status", "draft", "confirmed");
    assert_eq!(content, format!("{plan} status draft -> confirmed"));

    // A confirmed plan starts at `auto` `off`: its first level that is not `off` is the
    // resume.
    radian_arc::store::plan_ops::set_auto(&store, &plan, "run").expect("the plan runs");
    let event = session.next_event().await;
    expect_plan_event(&event, &plan, "paused", "true", "false");

    // A change between two levels that are not `off` is not a pause.
    radian_arc::store::plan_ops::set_auto(&store, &plan, "merge").expect("the plan merges");
    session.no_event_for(Duration::from_secs(11)).await;

    // `radian arc pause` sets `auto` to `off` and records no reason.
    radian_arc::store::plan_ops::pause(&store, &plan).expect("the plan pauses");
    let event = session.next_event().await;
    let content = expect_plan_event(&event, &plan, "paused", "false", "true");
    assert!(!content.contains('\n'), "no reason: {content:?}");

    // The next level resumes it.
    radian_arc::store::plan_ops::set_auto(&store, &plan, "run").expect("the plan runs again");
    let event = session.next_event().await;
    expect_plan_event(&event, &plan, "paused", "true", "false");

    // The automatic rework limit pauses the plan and records the reason beside the task.
    set_state(&scratch, &task, TaskState::Review, "rework_pending");
    let event = session.next_event().await;
    expect_event(
        &event,
        &task,
        &plan,
        "review",
        "rework_pending",
        "todo.ready",
    );
    store
        .change_task(&task, |task| {
            task.auto_rework = 2;
            Ok(())
        })
        .expect("the task has used its automatic reworks");
    let paused = radian_arc::store::attempt_ops::record_auto_rework(&store, &task)
        .expect("the automatic rework is counted")
        .expect("the plan pauses at the limit");
    let event = session.next_event().await;
    let content = expect_plan_event(&event, &plan, "paused", "false", "true");
    assert_eq!(
        content,
        format!("{plan} paused false -> true\n{}", paused.text)
    );

    session.close().await;
}

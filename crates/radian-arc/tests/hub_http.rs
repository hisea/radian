//! The hub's HTTP API against a real process and real repositories (ADR 0048 decision 16).
//!
//! Every test starts `radian hub` on port `0` with a temporary `RADIAN_HOME`, reads the bound
//! port from its first stdout line, and speaks HTTP/1.1 to it on loopback. The read routes are
//! compared with the CLI's own `--json` output, which is the contract: the route returns the
//! bytes `radian arc list --json` and `radian arc show --json` print.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Repo, radian_bin, write};
use fs2::FileExt;
use radian_arc::gate::Verdict;
use radian_arc::hub::Registry;
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config, plan_ops};
use serde_json::Value;

/// A `radian` command with the same isolated environment the hub uses.
fn radian(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(radian_bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("RADIAN_HOME", home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("radian runs")
}

/// A running hub. Dropping it kills the process.
struct Hub {
    child: Child,
    _stdout: BufReader<std::process::ChildStdout>,
    home: PathBuf,
    stderr_path: PathBuf,
    port: u16,
}

impl Hub {
    /// Starts `radian hub --port 0` and reads the bound port from the first stdout line.
    fn start(home: &Path) -> Self {
        fs::create_dir_all(home).expect("the hub home is creatable");
        let stderr_path = home.join("hub.stderr.log");
        let stderr = fs::File::create(&stderr_path).expect("the hub stderr file is creatable");
        let mut child = Command::new(radian_bin())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("RADIAN_HOME", home)
            .args(["hub", "--port", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("radian hub starts");
        let stdout = child.stdout.take().expect("the hub has stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the hub's first stdout line reads");
        let first = line.trim_end();
        let port: u16 = first
            .strip_prefix("hub: listening on 127.0.0.1:")
            .unwrap_or_else(|| panic!("the first stdout line names the listener: {first:?}"))
            .parse()
            .expect("the first stdout line names a port");
        Self {
            child,
            _stdout: reader,
            home: home.to_path_buf(),
            stderr_path,
            port,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn token(&self) -> String {
        fs::read_to_string(self.home.join("hub/token"))
            .expect("the token file exists")
            .trim()
            .to_owned()
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn request(&self, method: &str, path: &str, header: Option<&str>) -> Response {
        http(self.port, method, path, header)
    }

    /// A `POST` on a write route with a JSON body.
    fn post(&self, name: &str, id: &str, action: &str, body: &str) -> Response {
        let path = format!("/v1/repos/{name}/items/{id}/{action}");
        http_with_body(
            self.port,
            "POST",
            &path,
            Some(&format!("Bearer {}", self.token())),
            Some(body),
        )
    }

    fn authorized(&self, method: &str, path: &str) -> Response {
        self.request(method, path, Some(&format!("Bearer {}", self.token())))
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One HTTP/1.1 response, split at the first blank line.
#[derive(Debug)]
struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("the body is JSON: {error}: {:?}", self.body))
    }
}

/// One request over a fresh connection. `header` is the full `Authorization` value, so a test
/// can send `Basic <token>` as well as `Bearer <token>`.
fn http(port: u16, method: &str, path: &str, header: Option<&str>) -> Response {
    http_with_body(port, method, path, header, None)
}

/// One request with an optional JSON body, over a fresh connection.
fn http_with_body(
    port: u16,
    method: &str,
    path: &str,
    header: Option<&str>,
    body: Option<&str>,
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the hub accepts a connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout is settable");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(header) = header {
        request.push_str(&format!("Authorization: {header}\r\n"));
    }
    let body = body.unwrap_or("");
    request.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    ));
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the response reads");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("the response has a header block: {:?}", raw));
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("the status line has a code")
        .parse()
        .expect("the status code is a number");
    Response {
        status,
        body: raw[split + 4..].to_vec(),
    }
}

/// The hub home inside a repository's scratch root.
fn hub_home(repo: &Repo) -> PathBuf {
    let home = repo.root.join("hub-home");
    fs::create_dir_all(&home).expect("the hub home is creatable");
    home
}

/// Registers `repo` under `name` in `home`'s registry.
fn register(home: &Path, name: &str, repo: &Repo) {
    Registry::at(home.join("hub"))
        .add(&repo.clone, Some(name))
        .expect("the repository registers");
}

/// An initialized arc store in the repository's clone.
fn arc_store(repo: &Repo) -> Store {
    let root = repo.arc();
    fs::create_dir_all(&root).expect("the arc directory is creatable");
    fs::write(root.join("config.toml"), config::default_text("main")).expect("the config writes");
    Store::at(repo.clone.clone())
}

fn add_task(store: &Store, slug: &str, title: &str) -> (String, String) {
    let plan = store
        .add_plan("hub", "the hub plan", "a request\n")
        .expect("the plan adds");
    let task = store
        .add_task(
            &plan,
            slug,
            title,
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("the task adds");
    (plan, task)
}

fn set_attempt(store: &Store, task: &str, base: &str, head: &str) {
    store
        .change_task(task, |task| {
            task.state = TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            task.attempt = 1;
            task.base_sha = Some(base.to_owned());
            task.head_sha = Some(head.to_owned());
            Ok(())
        })
        .expect("the task changes");
}

fn commit(repo: &Repo, path: &str, content: &str, message: &str) -> String {
    write(&repo.clone.join(path), content);
    repo.git(&["add", path]);
    repo.git(&["commit", "-q", "-m", message]);
    repo.git(&["rev-parse", "HEAD"]).trim().to_owned()
}

/// A plan and a task in a fresh repository. Two repositories built this way produce the same
/// ids and the same files, so an action through the hub and the same action through the CLI
/// can be compared byte for byte.
fn setup(repo: &Repo) -> (Store, String, String) {
    let store = arc_store(repo);
    let (plan, task) = add_task(&store, "one", "the one task");
    (store, plan, task)
}

/// Every regular file under `.radian/arc`, by its path relative to that directory.
///
/// `runner.lock` is left out: it is not a state file, it carries the pid of whichever process
/// last took it, and the CLI's wrapper takes it to run pending hooks. The hub, like the TUI,
/// takes only `state.lock`, so its absence in the hub's copy is the rule, not a difference in
/// what the action wrote.
fn arc_files(repo: &Repo) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, files);
            } else if let Ok(bytes) = fs::read(&path) {
                let relative = path
                    .strip_prefix(root)
                    .expect("the file is under the arc directory")
                    .to_string_lossy()
                    .into_owned();
                if relative == "runner.lock" {
                    continue;
                }
                files.insert(relative, bytes);
            }
        }
    }
    let root = repo.arc();
    let mut files = BTreeMap::new();
    walk(&root, &root, &mut files);
    files
}

/// Puts a task in `review.awaiting_human`, which is all `reject` needs.
fn set_review(store: &Store, task: &str) {
    store
        .change_task(task, |task| {
            task.state = TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            Ok(())
        })
        .expect("the task is awaiting the owner");
}

/// Puts a task in `doing.running` for a refused merge and for a cancel.
fn set_doing(store: &Store, task: &str) {
    store
        .change_task(task, |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            Ok(())
        })
        .expect("the task is running");
}

/// A confirmed plan whose task is published on `work/<task>` and verified to
/// `review.awaiting_human`, ready for a real merge. Returns the head sha.
fn merge_ready(repo: &Repo, store: &Store, plan: &str, task: &str) -> String {
    plan_ops::confirm(store, plan).expect("the plan confirms");
    let base = repo.main_sha();
    let branch = format!("work/{task}");
    repo.git(&["checkout", "-B", &branch, &base]);
    write(&repo.clone.join("feature.txt"), "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "feature"]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", "origin", &branch]);
    store
        .change_task(task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .expect("the task is verifying");
    review::verify(store, task).expect("verification runs");
    head
}

/// Compares all of `.radian/arc` between the two repositories, naming the file that differs
/// and its text (the arc files are Markdown, JSON and the empty lock files).
fn assert_same_arc_files(a: &Repo, b: &Repo) {
    compare_files(&arc_files(a), &arc_files(b));
}

/// The state files: `config.toml`, `plans/`, `tasks/`, `ledger/` and the lock files, without
/// `runs/` or `gates/`. An action that records a timestamp in a run file (a reconcile's
/// `attempt_result.json`) cannot be byte-identical across two runs, so that test compares
/// these files, the transition, and the run file names.
fn state_files(repo: &Repo) -> BTreeMap<String, Vec<u8>> {
    arc_files(repo)
        .into_iter()
        .filter(|(name, _)| !name.starts_with("runs/") && !name.starts_with("gates/"))
        .collect()
}

fn assert_same_state_files(a: &Repo, b: &Repo) {
    compare_files(&state_files(a), &state_files(b));
}

fn compare_files(left: &BTreeMap<String, Vec<u8>>, right: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(
        left.keys().collect::<Vec<_>>(),
        right.keys().collect::<Vec<_>>(),
        "the two repositories hold different files"
    );
    for (name, bytes) in left {
        let other = right.get(name);
        if Some(bytes) == other {
            continue;
        }
        panic!(
            "`{name}` differs:\n  http: {:?}\n  cli:  {:?}",
            String::from_utf8_lossy(bytes),
            other.map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        );
    }
}

/// Takes `runner.lock` for a repository from the test process, so the CLI's owner command
/// (which runs its pending hooks only when it can take the lock without waiting) leaves them
/// for a loop, exactly as the hub route does. The lock is released when the file drops.
fn hold_runner_lock(repo: &Repo) -> std::fs::File {
    let path = repo.arc().join("runner.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .expect("runner.lock opens");
    file.lock_exclusive().expect("the test holds runner.lock");
    file
}

/// The task's `(state, phase)`.
fn task_state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

// ---- edge case 1 -------------------------------------------------------------------------

#[test]
fn the_list_and_show_routes_are_the_cli_json_bytes() {
    let a = Repo::new("hub-http-list-a");
    let b = Repo::new("hub-http-list-b");
    let store = arc_store(&a);
    let (_plan, task) = add_task(&store, "one", "the one task");
    // A second registered repository: `GET /v1/repos` names both, in file order.
    arc_store(&b);
    let home = hub_home(&a);
    register(&home, "a", &a);
    register(&home, "b", &b);
    let hub = Hub::start(&home);

    let listed = hub.authorized("GET", "/v1/repos/a/items");
    assert_eq!(listed.status, 200);
    let cli = a.radian(&["arc", "list", "--json"]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(
        listed.body,
        cli.stdout.as_bytes(),
        "the route returns exactly what the CLI prints"
    );

    let shown = hub.authorized("GET", &format!("/v1/repos/a/items/{task}"));
    assert_eq!(shown.status, 200);
    let cli = a.radian(&["arc", "show", &task, "--json"]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(shown.body, cli.stdout.as_bytes());

    let repos = hub.authorized("GET", "/v1/repos");
    assert_eq!(repos.status, 200);
    assert_eq!(
        repos.json(),
        serde_json::json!([
            {
                "name": "a",
                "path": fs::canonicalize(&a.clone).unwrap().display().to_string(),
                "scheduled": false,
                "lock_holder": null,
            },
            {
                "name": "b",
                "path": fs::canonicalize(&b.clone).unwrap().display().to_string(),
                "scheduled": false,
                "lock_holder": null,
            }
        ])
    );

    let version = hub.authorized("GET", "/v1/version");
    assert_eq!(version.status, 200);
    assert_eq!(
        version.json(),
        serde_json::json!({ "api_version": 1, "schema_version": 1 })
    );
}

// ---- edge case 2 -------------------------------------------------------------------------

#[test]
fn every_request_needs_a_bearer_token_and_a_bad_one_gets_an_empty_object() {
    let a = Repo::new("hub-http-token");
    arc_store(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let token = hub.token();

    for (path, header) in [
        ("/v1/version", None),
        ("/v1/repos", None),
        ("/v1/repos", Some("Bearer wrong".to_owned())),
        ("/v1/repos", Some(format!("Basic {token}"))),
        ("/v1/version", Some("Bearer".to_owned())),
    ] {
        let response = hub.request("GET", path, header.as_deref());
        assert_eq!(response.status, 401, "{path} {header:?}");
        assert_eq!(response.body, b"{}".to_vec(), "{path} {header:?}");
    }

    // The good token still works, and the server never wrote the token to stderr.
    assert_eq!(hub.authorized("GET", "/v1/version").status, 200);
    let stderr = hub.stderr();
    assert!(
        !stderr.contains(&token),
        "the token never reaches stderr: {stderr}"
    );
}

// ---- edge case 3 -------------------------------------------------------------------------

#[test]
fn a_rotated_token_takes_effect_on_the_next_request() {
    let a = Repo::new("hub-http-rotate");
    arc_store(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let old = hub.token();
    assert_eq!(
        hub.request("GET", "/v1/version", Some(&format!("Bearer {old}")))
            .status,
        200
    );

    let rotated = radian(&home, &["hub", "token", "rotate"]);
    assert_eq!(rotated.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&rotated.stdout), "rotated\n");
    let new = hub.token();
    assert_ne!(new, old, "rotate writes a new token");

    assert_eq!(
        hub.request("GET", "/v1/version", Some(&format!("Bearer {old}")))
            .status,
        401,
        "the old token fails on the next request"
    );
    assert_eq!(
        hub.request("GET", "/v1/version", Some(&format!("Bearer {new}")))
            .status,
        200,
        "the new token works without a restart"
    );
}

// ---- edge case 4 -------------------------------------------------------------------------

#[test]
fn the_report_is_the_newest_attempts_last_assistant_message() {
    let a = Repo::new("hub-http-report");
    let store = arc_store(&a);
    let (plan, task) = add_task(&store, "report", "the report task");
    for (attempt, text) in [(1, "the first report"), (2, "the newest report")] {
        let dir = a.run_dir(&task, attempt);
        fs::create_dir_all(&dir).expect("the run directory is creatable");
        fs::write(
            dir.join("result.jsonl"),
            format!(
                "{{\"type\":\"Result\",\"text\":\"{text}\"}}\n\
                 {{\"type\":\"Result\",\"text\":\"the last one of attempt {attempt}\"}}\n"
            ),
        )
        .expect("result.jsonl writes");
    }
    let (_plan, empty) = add_task(&store, "empty", "the empty task");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let report = hub.authorized("GET", &format!("/v1/repos/a/items/{task}/report"));
    assert_eq!(report.status, 200);
    assert_eq!(
        report.json(),
        serde_json::json!({
            "attempt": format!("{task}-2"),
            "text": "the last one of attempt 2",
        })
    );

    let none = hub.authorized("GET", &format!("/v1/repos/a/items/{empty}/report"));
    assert_eq!(none.status, 204);
    assert!(none.body.is_empty());

    // An id that names no task is the CLI's 404, not a task without an attempt; a plan's id
    // is not a task either.
    let unknown = hub.authorized("GET", "/v1/repos/a/items/TK9999-missing/report");
    assert_eq!(unknown.status, 404);
    assert_eq!(
        unknown.json(),
        serde_json::json!({"error": "no task `TK9999-missing`"})
    );
    let plan_only = hub.authorized("GET", &format!("/v1/repos/a/items/{plan}/report"));
    assert_eq!(plan_only.status, 404, "a plan id names no task");
    assert_eq!(
        plan_only.json(),
        serde_json::json!({"error": format!("no task `{plan}`")})
    );
}

// ---- edge case 5 -------------------------------------------------------------------------

#[test]
fn the_diff_route_lists_numstat_and_cuts_one_file_at_256_kib() {
    let a = Repo::new("hub-http-diff");
    let base = a.main_sha();
    a.git(&["checkout", "-q", "-B", "work/diff-test", &base]);
    commit(&a, "one.txt", "a\nb\n", "one file");
    let big = "x".repeat(300 * 1024);
    let head = commit(&a, "big.txt", &big, "the big file");
    a.git(&["checkout", "-q", "main"]);
    // The store is created after the branch commits, so `.radian/` never enters them.
    let store = arc_store(&a);
    let (_plan, task) = add_task(&store, "diff", "the diff task");
    set_attempt(&store, &task, &base, &head);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let listed = hub.authorized("GET", &format!("/v1/repos/a/items/{task}/diff"));
    assert_eq!(listed.status, 200);
    let files = listed.json();
    let files = files.as_array().expect("the diff list is an array");
    assert_eq!(files.len(), 2, "{files:?}");
    let one = files
        .iter()
        .find(|file| file["path"] == "one.txt")
        .expect("one.txt is listed");
    assert_eq!(one["added"], 2);
    assert_eq!(one["removed"], 0);
    let big = files
        .iter()
        .find(|file| file["path"] == "big.txt")
        .expect("big.txt is listed");
    assert_eq!(big["added"], 1);
    assert_eq!(big["removed"], 0);

    let body = hub.authorized("GET", &format!("/v1/repos/a/items/{task}/diff/big.txt"));
    assert_eq!(body.status, 200);
    let body = body.json();
    assert_eq!(body["path"], "big.txt");
    assert_eq!(body["truncated"], true);
    let text = body["text"].as_str().expect("the text is a string");
    assert_eq!(text.len(), 256 * 1024, "the text is cut exactly at 256 KiB");

    assert_eq!(
        hub.authorized("GET", &format!("/v1/repos/a/items/{task}/diff/missing.txt"))
            .status,
        404
    );
}

// ---- edge case 6 -------------------------------------------------------------------------

#[test]
fn an_unknown_repository_path_and_method_answer_404_and_405() {
    let a = Repo::new("hub-http-404");
    arc_store(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let unknown = hub.authorized("GET", "/v1/repos/b/items");
    assert_eq!(unknown.status, 404);
    assert_eq!(
        unknown.json(),
        serde_json::json!({"error": "no repository named `b` is registered"})
    );

    let route = hub.authorized("GET", "/v1/nothing");
    assert_eq!(route.status, 404);
    assert_eq!(route.json(), serde_json::json!({"error": "no such route"}));

    let method = hub.authorized("POST", "/v1/repos");
    assert_eq!(method.status, 405);
}

// a required status is `200` and the CLI's own text; the files are the CLI's bytes.
#[test]
fn a_confirm_through_the_route_writes_the_cli_bytes_and_returns_its_text() {
    let a = Repo::new("hub-write-confirm");
    let b = Repo::new("hub-write-confirm-copy");
    let (_sa, plan, task) = setup(&a);
    let (_sb, plan_b, task_b) = setup(&b);
    assert_eq!((&plan, &task), (&plan_b, &task_b), "the fixtures match");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let response = hub.post("a", &plan, "confirm", "{}");
    assert_eq!(response.status, 200, "{:?}", response.json());
    let body = response.json();
    assert_eq!(body["ok"], true);
    assert_eq!(body["text"], format!("{plan} confirmed"));

    let cli = b.radian(&["arc", "confirm", plan.as_str()]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(body["text"], cli.stdout.trim_end());
    assert_same_arc_files(&a, &b);
}

// ---- edge case 2: reject -----------------------------------------------------------------

#[test]
fn a_reject_with_an_empty_note_is_400_and_changes_nothing() {
    let a = Repo::new("hub-write-reject-empty");
    let (store, _plan, task) = setup(&a);
    set_review(&store, &task);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    for body in [r#"{"note": ""}"#, r#"{"note": "   "}"#, "{}"] {
        let response = hub.post("a", &task, "reject", body);
        assert_eq!(response.status, 400, "{body}: {:?}", response.json());
        assert_eq!(
            response.json(),
            serde_json::json!({"ok": false, "text": "the reject note is empty"})
        );
        assert_eq!(arc_files(&a), before, "{body} changed no file");
    }
}

#[test]
fn a_reject_with_a_note_writes_the_cli_rework_section() {
    let a = Repo::new("hub-write-reject");
    let b = Repo::new("hub-write-reject-copy");
    let (sa, _plan, task) = setup(&a);
    let (sb, _plan_b, task_b) = setup(&b);
    assert_eq!(task, task_b, "the fixtures match");
    set_review(&sa, &task);
    set_review(&sb, &task_b);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let note = "the tests do not cover the retry";
    let response = hub.post("a", &task, "reject", &format!(r#"{{"note": "{note}"}}"#));
    assert_eq!(response.status, 200, "{:?}", response.json());
    assert_eq!(
        response.json()["text"],
        format!("{task} review.rework_pending")
    );

    let _lock = hold_runner_lock(&b);
    let cli = b.radian(&["arc", "reject", task.as_str(), "--note", note]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(response.json()["text"], cli.stdout.trim_end());
    assert_same_arc_files(&a, &b);
}

// ---- edge case 3: auto -------------------------------------------------------------------

#[test]
fn an_auto_level_outside_the_set_is_422_and_merge_is_200() {
    let a = Repo::new("hub-write-auto");
    let b = Repo::new("hub-write-auto-copy");
    let (sa, plan, _task) = setup(&a);
    let (sb, plan_b, _task_b) = setup(&b);
    plan_ops::confirm(&sa, &plan).expect("the plan confirms");
    plan_ops::confirm(&sb, &plan_b).expect("the plan confirms");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    let response = hub.post("a", &plan, "auto", r#"{"level": "sometimes"}"#);
    assert_eq!(response.status, 422, "{:?}", response.json());
    let body = response.json();
    assert_eq!(body["ok"], false);
    assert!(
        body["text"]
            .as_str()
            .unwrap_or_default()
            .contains("levels are off, run, merge"),
        "the message names the set: {body}"
    );
    assert_eq!(arc_files(&a), before, "a refused value changes nothing");

    let response = hub.post("a", &plan, "auto", r#"{"level": "merge"}"#);
    assert_eq!(response.status, 200, "{:?}", response.json());
    assert_eq!(response.json()["text"], format!("{plan} auto merge"));
    let cli = b.radian(&["arc", "auto", plan.as_str(), "merge"]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_same_arc_files(&a, &b);
}

// ---- edge case 4: merge ------------------------------------------------------------------

#[test]
fn a_merge_through_the_route_reaches_done_merged_and_a_doing_task_is_409() {
    let a = Repo::new("hub-write-merge");
    let b = Repo::new("hub-write-merge-copy");
    let (sa, plan, task) = setup(&a);
    let (sb, plan_b, task_b) = setup(&b);
    let head_a = merge_ready(&a, &sa, &plan, &task);
    let head_b = merge_ready(&b, &sb, &plan_b, &task_b);
    assert_eq!(
        task_state(&sa, &task),
        ("review".to_owned(), "awaiting_human".to_owned())
    );
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let response = hub.post("a", &task, "merge", "{}");
    assert_eq!(response.status, 200, "{:?}", response.json());
    assert!(
        response.json()["text"]
            .as_str()
            .unwrap_or_default()
            .contains("done.merged"),
        "{:?}",
        response.json()
    );

    let _lock = hold_runner_lock(&b);
    let cli = b.radian(&["arc", "merge", task_b.as_str()]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert!(cli.stdout.contains("done.merged"), "{}", cli.stdout);

    assert_eq!(task_state(&sa, &task), task_state(&sb, &task_b));
    assert_eq!(
        task_state(&sa, &task),
        ("done".to_owned(), "merged".to_owned())
    );
    assert_eq!(a.remote_main_sha(), head_a);
    assert_eq!(b.remote_main_sha(), head_b);
    let merged_a = sa.load_task(&task).unwrap().item.merged_sha;
    let merged_b = sb.load_task(&task_b).unwrap().item.merged_sha;
    assert_eq!(merged_a.as_deref(), Some(head_a.as_str()));
    assert_eq!(merged_b.as_deref(), Some(head_b.as_str()));
}

#[test]
fn a_merge_on_a_doing_task_is_409_with_the_cli_sentence_and_changes_nothing() {
    let a = Repo::new("hub-write-merge-doing");
    let b = Repo::new("hub-write-merge-doing-copy");
    let (sa, _plan, task) = setup(&a);
    let (sb, _plan_b, task_b) = setup(&b);
    set_doing(&sa, &task);
    set_doing(&sb, &task_b);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    let response = hub.post("a", &task, "merge", "{}");
    assert_eq!(response.status, 409, "{:?}", response.json());
    let body = response.json();
    assert_eq!(body["ok"], false);
    let sentence =
        format!("task `{task}` is `doing.running`; only a `review.awaiting_human` task merges");
    assert_eq!(body["text"], sentence);
    assert_eq!(arc_files(&a), before, "a refused merge changes nothing");

    let cli = b.radian(&["arc", "merge", task_b.as_str()]);
    assert_eq!(cli.code, 3, "{cli:?}");
    assert!(cli.stderr.contains(sentence.as_str()), "{}", cli.stderr);
}

// ---- edge case 5: a held state.lock ------------------------------------------------------

#[test]
fn every_action_on_a_held_state_lock_is_409_with_no_file_change() {
    use std::fs::OpenOptions;

    let a = Repo::new("hub-write-locked");
    let (_store, plan, task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    // Hold the repository's `state.lock` from this process, exactly as a test process does.
    let lock_path = a.arc().join("state.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .expect("the lock file opens");
    lock.lock_exclusive().expect("the test holds state.lock");
    let before = arc_files(&a);

    let cases = [
        ("confirm", plan.as_str(), "{}"),
        ("merge", task.as_str(), "{}"),
        ("cancel", task.as_str(), "{}"),
        ("pause", plan.as_str(), "{}"),
        ("reverify", task.as_str(), "{}"),
        ("reject", task.as_str(), r#"{"note": "a note"}"#),
        ("auto", plan.as_str(), r#"{"level": "off"}"#),
        ("priority", plan.as_str(), r#"{"priority": "P1"}"#),
    ];
    for (action, id, body) in cases {
        let started = Instant::now();
        let response = hub.post("a", id, action, body);
        let elapsed = started.elapsed();
        assert_eq!(response.status, 409, "{action}: {:?}", response.json());
        assert_eq!(
            response.json(),
            serde_json::json!({
                "ok": false,
                "text": "state.lock is locked by another process",
            }),
            "{action}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "{action} waited {elapsed:?}, past the CLI's lock wait"
        );
        assert_eq!(
            arc_files(&a),
            before,
            "{action} changed a file through the lock"
        );
    }
    FileExt::unlock(&lock).expect("the test releases state.lock");
}

// ---- edge case 6: the routes that are not there ------------------------------------------

#[test]
fn start_reset_budget_and_other_actions_are_404_and_get_confirm_is_405() {
    let a = Repo::new("hub-write-no-route");
    let (_store, plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    for action in ["start", "reset-budget", "explode"] {
        let response = hub.post("a", &plan, action, "{}");
        assert_eq!(response.status, 404, "{action}: {:?}", response.json());
        assert_eq!(
            response.json(),
            serde_json::json!({"error": "no such route"}),
            "{action}"
        );
    }

    let response = hub.authorized("GET", &format!("/v1/repos/a/items/{plan}/confirm"));
    assert_eq!(response.status, 405);
    assert_eq!(
        response.json(),
        serde_json::json!({"error": "method not allowed"})
    );
}

// ---- edge case 7: an unknown repository or id --------------------------------------------

#[test]
fn an_action_on_an_unregistered_repository_or_unknown_id_is_404() {
    let a = Repo::new("hub-write-404");
    let (_store, _plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let response = hub.post("nope", "PL0001-x", "confirm", "{}");
    assert_eq!(response.status, 404, "{:?}", response.json());
    assert_eq!(
        response.json(),
        serde_json::json!({
            "ok": false,
            "text": "no repository named `nope` is registered",
        })
    );

    let response = hub.post("a", "TK9999-missing", "confirm", "{}");
    assert_eq!(response.status, 404, "{:?}", response.json());
    assert_eq!(
        response.json(),
        serde_json::json!({"ok": false, "text": "no plan or task `TK9999-missing`"})
    );

    let response = hub.post("a", "TK9999-missing", "merge", "{}");
    assert_eq!(response.status, 404, "{:?}", response.json());
    assert_eq!(
        response.json(),
        serde_json::json!({"ok": false, "text": "no task `TK9999-missing`"})
    );
}

// ---- edge case 8: priority ----------------------------------------------------------------

#[test]
fn a_priority_through_the_route_is_the_cli_priority() {
    let a = Repo::new("hub-write-priority");
    let b = Repo::new("hub-write-priority-copy");
    let (sa, plan, _task) = setup(&a);
    let (_sb, plan_b, _task_b) = setup(&b);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let response = hub.post("a", &plan, "priority", r#"{"priority": "P1"}"#);
    assert_eq!(response.status, 200, "{:?}", response.json());
    assert_eq!(response.json()["text"], format!("{plan} priority P1"));

    let cli = b.radian(&["arc", "priority", plan_b.as_str(), "P1"]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_same_arc_files(&a, &b);
    assert_eq!(
        sa.load_plan(&plan).unwrap().item.priority.as_deref(),
        Some("P1")
    );
}

#[test]
fn a_priority_outside_the_set_is_422_and_changes_nothing() {
    let a = Repo::new("hub-write-priority-bad");
    let (_store, plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    let response = hub.post("a", &plan, "priority", r#"{"priority": "P6"}"#);
    assert_eq!(response.status, 422, "{:?}", response.json());
    assert!(
        response.json()["text"]
            .as_str()
            .unwrap_or_default()
            .contains("priorities are P0, P1, P2, P3, P4, P5"),
        "{:?}",
        response.json()
    );
    assert_eq!(arc_files(&a), before);
}

// ---- edge case 9: cancel on a doing task --------------------------------------------------

#[test]
fn a_cancel_through_the_route_is_the_cli_cancel() {
    let a = Repo::new("hub-write-cancel");
    let b = Repo::new("hub-write-cancel-copy");
    let (sa, _plan, task) = setup(&a);
    let (sb, _plan_b, task_b) = setup(&b);
    set_doing(&sa, &task);
    set_doing(&sb, &task_b);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let response = hub.post("a", &task, "cancel", "{}");
    assert_eq!(response.status, 200, "{:?}", response.json());
    let text = response.json()["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let _lock = hold_runner_lock(&b);
    let cli = b.radian(&["arc", "cancel", task_b.as_str()]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(text, cli.stdout.trim_end());
    assert_eq!(task_state(&sa, &task), task_state(&sb, &task_b));
    // The cancel's own state files are byte-identical. Its run files are the same files, but
    // `attempt_result.json` records the reconcile's own clock, so two runs cannot carry the
    // same bytes there; the transition and the state files are what the action decides.
    assert_same_state_files(&a, &b);
    let run_files = |repo: &Repo| {
        arc_files(repo)
            .into_keys()
            .filter(|name| name.starts_with("runs/"))
            .collect::<Vec<_>>()
    };
    assert_eq!(run_files(&a), run_files(&b), "the same run files");
}

// ---- edge case 10: no token on a write route ----------------------------------------------

#[test]
fn a_write_route_without_a_token_is_401_and_changes_nothing() {
    let a = Repo::new("hub-write-token");
    let (_store, plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    let path = format!("/v1/repos/a/items/{plan}/confirm");
    let response = hub.request("POST", &path, None);
    assert_eq!(response.status, 401);
    assert_eq!(response.body, b"{}".to_vec());
    assert_eq!(arc_files(&a), before, "nothing changed without a token");
}

// ---- the token file ----------------------------------------------------------------------

#[test]
fn the_token_file_is_0600_and_a_missing_one_is_created_at_start() {
    use std::os::unix::fs::PermissionsExt;

    let a = Repo::new("hub-http-token-file");
    let home = hub_home(&a);
    let path = home.join("hub/token");
    assert!(!path.exists(), "the fixture starts without a token");
    let hub = Hub::start(&home);
    assert!(path.is_file(), "the hub creates the token at start");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the token file is 0600");
    let token = hub.token();
    assert_eq!(token.len(), 64);
    assert!(
        token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "the token is lowercase hex: {token}"
    );
}

// ---- edge case 10 ------------------------------------------------------------------------

#[test]
fn a_second_hub_is_refused_by_the_lock_and_a_port_in_use_names_the_port() {
    let a = Repo::new("hub-http-lock");
    arc_store(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let second = radian(&home, &["hub", "--port", "0"]);
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("hub.lock") && stderr.contains(&hub.pid().to_string()),
        "the refusal names hub.lock and the holder's pid: {stderr}"
    );

    // A port another program holds is a startup error that names the port. A fresh home has
    // no lock, so the bind is the failure the test sees.
    let holder = TcpListener::bind("127.0.0.1:0").expect("the test holds a port");
    let port = holder.local_addr().unwrap().port();
    let other_home = a.root.join("other-home");
    fs::create_dir_all(&other_home).unwrap();
    let blocked = radian(&other_home, &["hub", "--port", &port.to_string()]);
    assert_eq!(blocked.status.code(), Some(1), "{blocked:?}");
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains(&port.to_string()),
        "the startup error names the port: {stderr}"
    );
}

// ---- the verdicts route ------------------------------------------------------------------

#[test]
fn the_verdicts_route_reads_the_newest_attempts_verdicts() {
    let a = Repo::new("hub-http-verdicts");
    let store = arc_store(&a);
    let (plan, task) = add_task(&store, "verdict", "the verdict task");
    let dir = a.run_dir(&task, 1).join("verdicts");
    fs::create_dir_all(&dir).expect("the verdict directory is creatable");
    let path = dir.join("human.json");
    Verdict::human("a".repeat(40), true, None)
        .write_new(&path)
        .expect("the verdict writes");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    let response = hub.authorized("GET", &format!("/v1/repos/a/items/{task}/verdicts"));
    assert_eq!(response.status, 200);
    let list = response.json();
    let list = list.as_array().expect("the verdict list is an array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["stage"], "human");
    assert_eq!(list[0]["result"], "pass");
    assert!(list[0]["reason"].is_null());
    assert_eq!(list[0]["path"], path.display().to_string());
    assert_eq!(list[0]["verdict"]["stage"], "human");
    assert_eq!(list[0]["verdict"]["result"], "pass");

    // An id that names no task is the CLI's 404, not an empty list; a plan's id is not a task
    // either.
    let unknown = hub.authorized("GET", "/v1/repos/a/items/TK9999-missing/verdicts");
    assert_eq!(unknown.status, 404);
    assert_eq!(
        unknown.json(),
        serde_json::json!({"error": "no task `TK9999-missing`"})
    );
    let plan_only = hub.authorized("GET", &format!("/v1/repos/a/items/{plan}/verdicts"));
    assert_eq!(plan_only.status, 404, "a plan id names no task");
    assert_eq!(
        plan_only.json(),
        serde_json::json!({"error": format!("no task `{plan}`")})
    );
}

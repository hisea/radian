//! The hub's HTTP API against a real process and real repositories (ADR 0048 decision 16).
//!
//! Most tests start `radian hub` on port `0` with a temporary `RADIAN_HOME`, read the bound
//! port from its first stdout line, and speak HTTP/1.1 to it on loopback. The rebind test
//! starts its first hub on port `0` too and its replacement on the port the first one reported.
//! The read routes are compared with the CLI's own `--json` output, which is the contract: the
//! route returns the bytes `radian arc list --json` and `radian arc show --json` print.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Repo, radian_bin, write};
use fs2::FileExt;
use radian_arc::gate::Verdict;
use radian_arc::hub::Registry;
use radian_arc::review;
use radian_arc::store::model::TaskState;
use radian_arc::store::{LOCK_WAIT, Store, config, plan_ops};
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
    stdout: BufReader<std::process::ChildStdout>,
    home: PathBuf,
    stderr_path: PathBuf,
    address: SocketAddr,
    port: u16,
}

impl Hub {
    /// Starts `radian hub --port 0` and reads the bound port from the first stdout line.
    fn start(home: &Path) -> Self {
        Self::start_with(home, &[])
    }

    /// [`Hub::start`] with extra environment variables, which is how the event-stream tests
    /// shorten the poll and keep-alive intervals.
    fn start_with(home: &Path, envs: &[(&str, &str)]) -> Self {
        Self::start_on_with(home, 0, envs)
    }

    /// Starts a hub on `port` (`0` asks the OS for one) and reads the bound port from the
    /// first stdout line.
    fn start_on(home: &Path, port: u16) -> Self {
        Self::start_on_with(home, port, &[])
    }

    fn start_on_with(home: &Path, port: u16, envs: &[(&str, &str)]) -> Self {
        Self::start_with_args(home, port, envs, &[])
    }

    /// [`Hub::start`] with a top-level `--config` file, so a test can prove the hub's loop reads
    /// the same configuration the port came from.
    fn start_config(home: &Path, config: &Path) -> Self {
        let config = config.to_str().expect("the config path is UTF-8");
        Self::start_with_args(home, 0, &[], &["--config", config])
    }

    /// Starts a hub whose `--config` file holds `config_text`, on port `0`. The file is written
    /// inside `home`.
    fn start_with_config(home: &Path, config_text: &str) -> Self {
        fs::create_dir_all(home).expect("the hub home is creatable");
        let config = home.join("config.toml");
        fs::write(&config, config_text).expect("the hub configuration writes");
        let config = config.to_str().expect("the config path is UTF-8");
        Self::start_with_args(home, 0, &[], &["--config", config])
    }

    fn start_with_args(home: &Path, port: u16, envs: &[(&str, &str)], leading: &[&str]) -> Self {
        fs::create_dir_all(home).expect("the hub home is creatable");
        let stderr_path = home.join("hub.stderr.log");
        let stderr = fs::File::create(&stderr_path).expect("the hub stderr file is creatable");
        let port_arg = port.to_string();
        let mut command = Command::new(radian_bin());
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("RADIAN_HOME", home)
            .args(leading)
            .args(["hub", "--port", port_arg.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("radian hub starts");
        let stdout = child.stdout.take().expect("the hub has stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the hub's first stdout line reads");
        let first = line.trim_end();
        let address: SocketAddr = first
            .strip_prefix("hub: listening on ")
            .unwrap_or_else(|| panic!("the first stdout line names the listener: {first:?}"))
            .parse()
            .expect("the first stdout line names an address and a port");
        let port = address.port();
        Self {
            child,
            stdout: reader,
            home: home.to_path_buf(),
            stderr_path,
            address,
            port,
        }
    }

    /// The next stdout line, trimmed of its newline.
    fn next_line(&mut self) -> String {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("the hub's stdout line reads");
        line.trim_end().to_owned()
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Stops the hub with SIGTERM and waits for it, so an event stream sees the connection
    /// end.
    fn terminate(&mut self) -> std::process::ExitStatus {
        let result = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        assert_eq!(result, 0, "SIGTERM reaches the hub");
        self.child.wait().expect("the hub exits")
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
    http_at(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
        method,
        path,
        header,
        body,
    )
}

/// One request to `address` on `port`, over a fresh connection.
fn http_at(
    address: IpAddr,
    port: u16,
    method: &str,
    path: &str,
    header: Option<&str>,
    body: Option<&str>,
) -> Response {
    let mut stream =
        TcpStream::connect(SocketAddr::new(address, port)).expect("the hub accepts a connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout is settable");
    let host = match address {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n");
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
    read_response(&mut stream)
}

/// Reads one response from `stream`, which the caller has already written to.
fn read_response(stream: &mut TcpStream) -> Response {
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the response reads");
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Response {
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

/// Sends `request` as given and then reads the response within `timeout`, writing no body after
/// the header block. Used to prove the hub answers before a declared body arrives.
fn http_head_only(port: u16, request: &str, timeout: Duration) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the hub accepts a connection");
    stream
        .set_read_timeout(Some(timeout))
        .expect("a read timeout is settable");
    stream
        .write_all(request.as_bytes())
        .expect("the request headers are written");
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .expect("the response arrives before the body is sent");
    parse_response(&raw)
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
/// last took it, and the CLI's wrapper takes it to run pending hooks. The hub's scheduling loop
/// holds it for its whole life, and a route action takes only `state.lock`, so its absence in
/// either copy is the rule, not a difference in what the action wrote.
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

/// Takes `runner.lock` for a repository from the test process. Held before a `radian hub`
/// starts, it makes the hub only read that repository instead of running its pass, so a route
/// test meets a store nothing else moves. For the CLI it means the owner command (which runs its
/// pending hooks only when it can take the lock without waiting) leaves them for a loop, exactly
/// as the hub route does. The lock is released when the file drops.
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
    // The route carries the board's computed column with the CLI's own bytes.
    let body = String::from_utf8(listed.body.clone()).expect("the list body is UTF-8");
    assert!(
        body.contains("\"board_column\": \"todo\""),
        "the new field is in the route body: {body}"
    );

    let shown = hub.authorized("GET", &format!("/v1/repos/a/items/{task}"));
    assert_eq!(shown.status, 200);
    let cli = a.radian(&["arc", "show", &task, "--json"]);
    assert_eq!(cli.code, 0, "{}", cli.stderr);
    assert_eq!(shown.body, cli.stdout.as_bytes());
    let body = String::from_utf8(shown.body.clone()).expect("the show body is UTF-8");
    assert!(
        body.contains("\"board_column\": \"todo\""),
        "the show route carries the column too: {body}"
    );

    // The scheduling loop takes each repository's `runner.lock`; wait for both, then the
    // route reports them as scheduled by this hub.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let repos = hub.authorized("GET", "/v1/repos").json();
        let scheduled = repos.as_array().is_some_and(|list| {
            list.iter()
                .all(|repo| repo["scheduled"] == serde_json::json!(true))
        });
        if scheduled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the hub never scheduled both repositories: {repos}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let repos = hub.authorized("GET", "/v1/repos");
    assert_eq!(repos.status, 200);
    assert_eq!(
        repos.json(),
        serde_json::json!([
            {
                "name": "a",
                "path": fs::canonicalize(&a.clone).unwrap().display().to_string(),
                "scheduled": true,
                "lock_holder": hub.pid().to_string(),
            },
            {
                "name": "b",
                "path": fs::canonicalize(&b.clone).unwrap().display().to_string(),
                "scheduled": true,
                "lock_holder": hub.pid().to_string(),
            }
        ])
    );

    let version = hub.authorized("GET", "/v1/version");
    assert_eq!(version.status, 200);
    // Version 3 adds the session close route; every route this test read keeps its bytes.
    assert_eq!(
        version.json(),
        serde_json::json!({ "api_version": 3, "schema_version": 1 })
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
            elapsed < LOCK_WAIT + Duration::from_secs(2),
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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
    let _read_only = hold_runner_lock(&a);
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

// ---- reverify: a verification that refuses -------------------------------------------------

#[test]
fn a_reverify_whose_verification_refuses_returns_the_claims_line_and_its_error() {
    let a = Repo::new("hub-write-reverify-fail");
    let b = Repo::new("hub-write-reverify-fail-copy");
    let (sa, _plan, task) = setup(&a);
    let (sb, _plan_b, task_b) = setup(&b);
    let base_a = a.main_sha();
    let base_b = b.main_sha();
    // No `work/<task>` branch is pushed, so the claim succeeds and the verification refuses
    // with the moved-branch error the CLI prints after the claim's line.
    set_attempt(&sa, &task, &base_a, &base_a);
    set_attempt(&sb, &task_b, &base_b, &base_b);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let _read_only = hold_runner_lock(&a);
    let hub = Hub::start(&home);

    let response = hub.post("a", &task, "reverify", "{}");
    assert_eq!(response.status, 409, "{:?}", response.json());
    let body = response.json();
    assert_eq!(body["ok"], false);
    let text = body["text"].as_str().expect("the body carries the text");
    assert!(
        text.starts_with(&format!("{task}-2 reverify -> review.verifying")),
        "the claim's line comes first: {text}"
    );
    assert!(
        text.contains("no branch for `work/"),
        "the verification's own error follows: {text}"
    );

    let _lock = hold_runner_lock(&b);
    let cli = b.radian(&["arc", "reverify", task_b.as_str()]);
    assert_eq!(cli.code, 3, "{cli:?}");
    let message = cli
        .stderr
        .trim_end()
        .strip_prefix("radian arc: ")
        .expect("the CLI prefixes its error");
    // The two repositories are separate clones, so their base commits differ; the route's
    // message names the hub repository's sha where the CLI's names the copy's.
    assert_eq!(
        text.replace(&base_a, &base_b),
        format!("{}\n{message}", cli.stdout.trim_end()),
        "the route's text is the CLI's claim line and error"
    );
    assert_eq!(
        task_state(&sa, &task),
        ("review".to_owned(), "verifying".to_owned()),
        "a refused verification leaves the claimed task verifying"
    );
}

// ---- edge case 10: no token on a write route ----------------------------------------------

#[test]
fn a_write_route_without_a_token_is_401_and_changes_nothing() {
    let a = Repo::new("hub-write-token");
    let (_store, plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let _read_only = hold_runner_lock(&a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    let path = format!("/v1/repos/a/items/{plan}/confirm");
    let response = hub.request("POST", &path, None);
    assert_eq!(response.status, 401);
    assert_eq!(response.body, b"{}".to_vec());
    assert_eq!(arc_files(&a), before, "nothing changed without a token");
}

// ---- the request body's limit and the order of authorization ------------------------------

#[test]
fn an_unauthorized_request_is_answered_before_its_body_is_read() {
    let a = Repo::new("hub-body-auth-first");
    let (_store, plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);

    // The body is declared and never sent. A hub that read the body first would wait for it,
    // and the read below would time out.
    let request = format!(
        "POST /v1/repos/a/items/{plan}/confirm HTTP/1.1\r\n\
         Host: 127.0.0.1:{}\r\nConnection: close\r\nContent-Type: application/json\r\n\
         Content-Length: 100000000\r\n\r\n",
        hub.port,
    );
    let response = http_head_only(hub.port, &request, Duration::from_secs(5));
    assert_eq!(response.status, 401);
    assert_eq!(response.body, b"{}".to_vec());
}

#[test]
fn a_body_over_the_limit_is_413_and_changes_nothing() {
    let a = Repo::new("hub-body-limit");
    let (store, _plan, task) = setup(&a);
    set_review(&store, &task);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let _read_only = hold_runner_lock(&a);
    let hub = Hub::start(&home);
    let before = arc_files(&a);

    let oversized = format!(
        "{{\"note\": \"{}\"}}",
        "x".repeat(radian_arc::hub::http::REQUEST_BODY_LIMIT)
    );
    let response = hub.post("a", &task, "reject", &oversized);
    assert_eq!(response.status, 413, "{:?}", response.json());
    assert_eq!(
        response.json(),
        serde_json::json!({"error": "request body too large"})
    );
    assert_eq!(arc_files(&a), before, "an oversized body changes nothing");
}

/// A large body does not change the route's answer: the route and the method are decided, and a
/// read route's body is never read, before any body is buffered.
#[test]
fn an_oversized_body_does_not_change_a_404_a_405_or_a_read_route() {
    let a = Repo::new("hub-body-route-first");
    let (_store, plan, _task) = setup(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let token = hub.token();
    let host = format!("127.0.0.1:{}", hub.port);
    let length = radian_arc::hub::http::REQUEST_BODY_LIMIT + 1;

    let request = format!(
        "POST /v1/repos/a/items/{plan}/start HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {length}\r\n\r\n"
    );
    let response = http_head_only(hub.port, &request, Duration::from_secs(5));
    assert_eq!(response.status, 404, "{:?}", response.json());

    let request = format!(
        "GET /v1/repos/a/items/{plan}/confirm HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {length}\r\n\r\n"
    );
    let response = http_head_only(hub.port, &request, Duration::from_secs(5));
    assert_eq!(response.status, 405, "{:?}", response.json());

    let request = format!(
        "GET /v1/version HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {length}\r\n\r\n"
    );
    let response = http_head_only(hub.port, &request, Duration::from_secs(5));
    assert_eq!(response.status, 200, "{:?}", response.json());
}

/// A declared `Content-Length` over the limit is refused before the client sends the body at
/// all, so a huge or endless body cannot keep a request task reading.
#[test]
fn a_declared_content_length_over_the_limit_is_413_before_the_body() {
    let a = Repo::new("hub-body-declared");
    let (store, _plan, task) = setup(&a);
    set_review(&store, &task);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let token = hub.token();
    let host = format!("127.0.0.1:{}", hub.port);
    let length = radian_arc::hub::http::REQUEST_BODY_LIMIT + 1;
    let request = format!(
        "POST /v1/repos/a/items/{task}/reject HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {length}\r\n\r\n"
    );
    let response = http_head_only(hub.port, &request, Duration::from_secs(5));
    assert_eq!(response.status, 413, "{:?}", response.json());
    assert_eq!(
        response.json(),
        serde_json::json!({"error": "request body too large"})
    );
}

/// The drain of an over-limit body has one deadline for the whole body, not a timeout per frame:
/// a client that declares an over-limit body and then sends a byte at a time cannot keep the
/// request reading past it.
#[test]
fn a_dribbled_over_limit_body_does_not_extend_the_drain() {
    let a = Repo::new("hub-body-dribble");
    let (store, _plan, task) = setup(&a);
    set_review(&store, &task);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start(&home);
    let token = hub.token();
    let host = format!("127.0.0.1:{}", hub.port);
    let length = radian_arc::hub::http::REQUEST_BODY_LIMIT + 1;

    let mut stream =
        TcpStream::connect(("127.0.0.1", hub.port)).expect("the hub accepts a connection");
    // Longer than the drain's whole deadline, short enough that the old per-frame timeout shows
    // as a read failure instead of a hang.
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("a read timeout is settable");
    let request = format!(
        "POST /v1/repos/a/items/{task}/reject HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: {length}\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .expect("the request headers are written");

    let mut writer = stream.try_clone().expect("the stream clones");
    let dribble = std::thread::spawn(move || {
        for _ in 0..30 {
            if writer.write_all(b"x").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let response = read_response(&mut stream);
    dribble.join().expect("the dribbling writer ends");

    assert_eq!(response.status, 413, "{:?}", response.json());
    assert_eq!(
        response.json(),
        serde_json::json!({"error": "request body too large"})
    );
}

/// The loop reads the same configuration file the port came from: a top-level `--config` sets
/// `[hub] concurrency` in the status line, and not the default file's value.
#[test]
fn the_hub_reads_the_explicit_config_file_for_the_loop() {
    let a = Repo::new("hub-http-explicit-config");
    arc_store(&a);
    let home = hub_home(&a);
    let config = a.root.join("explicit.toml");
    fs::write(
        &config,
        "schema_version = 1\n[hub]\nport = 7000\nconcurrency = 7\n",
    )
    .expect("the test writes its configuration");
    let mut hub = Hub::start_config(&home, &config);
    let mut status = String::new();
    for _ in 0..50 {
        let line = hub.next_line();
        if line.starts_with("hub: ") {
            status = line;
            break;
        }
    }
    assert!(
        status.contains("cap 7"),
        "the hub's loop reads `--config` too: {status:?}"
    );
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

/// A hub binds its port again after its predecessor's last connection left the port in
/// `TIME_WAIT`. The first hub starts on port `0` and the replacement uses the port it reported,
/// so the two binds are the same port with no probe in between; `bind_listener` sets
/// `SO_REUSEADDR` on its own socket before `bind`.
#[test]
fn a_hub_binds_its_port_again_while_the_last_connection_is_in_time_wait() {
    let a = Repo::new("hub-http-rebind");
    arc_store(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let mut first = Hub::start_on(&home, 0);
    let port = first.port;
    let response = first.authorized("GET", "/v1/version");
    assert_eq!(response.status, 200, "the first hub serves the port");
    // The request sent `Connection: close`, so the server closed first and `port` now has a
    // connection in `TIME_WAIT`.
    assert_eq!(
        first.terminate().code(),
        Some(0),
        "the first hub stops cleanly"
    );

    let mut second = Hub::start_on(&home, port);
    let response = second.authorized("GET", "/v1/version");
    assert_eq!(
        response.status, 200,
        "the replacement hub binds and serves the same port"
    );
    assert_eq!(
        second.terminate().code(),
        Some(0),
        "the replacement hub stops cleanly"
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

// ---- the event stream ---------------------------------------------------------------------

/// The hub's event stream as a client sees it: the status and headers are checked when it
/// opens, then each line of the chunked body is decoded as it arrives.
struct EventStream {
    reader: BufReader<TcpStream>,
    pending: Vec<u8>,
    done: bool,
}

impl EventStream {
    /// Opens `GET /v1/events` with the hub's token and checks the status line and content
    /// type before returning the body reader.
    fn open(hub: &Hub) -> Self {
        let mut stream =
            TcpStream::connect(("127.0.0.1", hub.port)).expect("the hub accepts a connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("a read timeout is settable");
        let request = format!(
            "GET /v1/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
            hub.port,
            hub.token()
        );
        stream
            .write_all(request.as_bytes())
            .expect("the request is written");
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader
            .read_line(&mut status)
            .expect("the event response's status line reads");
        assert!(status.contains(" 200 "), "the stream opens: {status:?}");
        let mut event_stream = false;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("the event response's header block reads");
            if line.is_empty() || line == "\r\n" || line == "\n" {
                break;
            }
            let header = line.to_ascii_lowercase();
            if header.starts_with("content-type:") {
                event_stream = header.contains("text/event-stream");
            }
        }
        assert!(event_stream, "the content type is text/event-stream");
        Self {
            reader,
            pending: Vec::new(),
            done: false,
        }
    }

    /// Reads one transfer-coding chunk into `pending`; `false` at the end of the body.
    fn fill(&mut self) -> bool {
        if self.done {
            return false;
        }
        loop {
            let mut size_line = String::new();
            match self.reader.read_line(&mut size_line) {
                Ok(0) => {
                    self.done = true;
                    return false;
                }
                Ok(_) => {}
                Err(error) => panic!("the stream's chunk header reads: {error}"),
            }
            let text = size_line.trim();
            if text.is_empty() {
                continue;
            }
            let size = usize::from_str_radix(text, 16)
                .unwrap_or_else(|_| panic!("the chunk size is hex: {text:?}"));
            if size == 0 {
                loop {
                    let mut line = String::new();
                    match self.reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) => panic!("the stream's trailer reads: {error}"),
                    }
                    if line.is_empty() || line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                self.done = true;
                return false;
            }
            let mut chunk = vec![0u8; size];
            self.reader
                .read_exact(&mut chunk)
                .expect("a whole chunk arrives");
            let mut ending = [0u8; 2];
            self.reader
                .read_exact(&mut ending)
                .expect("the chunk's CRLF arrives");
            self.pending.extend(chunk);
            return true;
        }
    }

    /// The next body line without its ending, or `None` at the end of the stream.
    fn line(&mut self) -> Option<String> {
        loop {
            if let Some(at) = self.pending.iter().position(|byte| *byte == b'\n') {
                let bytes: Vec<u8> = self.pending.drain(..=at).collect();
                let mut text = String::from_utf8(bytes).expect("the stream is UTF-8");
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
                return Some(text);
            }
            if !self.fill() {
                if self.pending.is_empty() {
                    return None;
                }
                let bytes = std::mem::take(&mut self.pending);
                let mut text = String::from_utf8(bytes).expect("the stream is UTF-8");
                if text.ends_with('\r') {
                    text.pop();
                }
                return Some(text);
            }
        }
    }

    /// The next `event: transition` and its JSON data, skipping keep-alive comments.
    fn event(&mut self) -> Value {
        loop {
            let line = self
                .line()
                .expect("the stream carries an event before it ends");
            if line == "event: transition" {
                let data = self
                    .line()
                    .expect("the event has a data line")
                    .strip_prefix("data: ")
                    .expect("the event's data line starts with `data: `")
                    .to_owned();
                assert_eq!(
                    self.line().as_deref(),
                    Some(""),
                    "an event ends with a blank line"
                );
                return serde_json::from_str(&data)
                    .unwrap_or_else(|error| panic!("the event data is JSON ({error}): {data}"));
            }
            assert!(
                !line.starts_with("data:"),
                "a data line without its event: {line:?}"
            );
        }
    }

    /// Reads until one keep-alive comment arrives, failing on any event in between.
    fn keep_alive(&mut self) {
        self.keep_alives(1);
    }

    /// Reads until `count` keep-alive comments arrive, failing on any event in between. The
    /// stream's `: open` comment is skipped like any other comment.
    fn keep_alives(&mut self, count: usize) {
        let mut seen = 0;
        while seen < count {
            let line = self.line().expect("the stream keeps itself alive");
            if line == ": keep-alive" {
                seen += 1;
            } else if line.is_empty() || line.starts_with(':') {
                // The blank line that ends the previous frame, or the `: open` comment.
            } else {
                panic!("expected a keep-alive comment, got {line:?}");
            }
        }
    }
}

/// A hub whose event poll and keep-alive run every second: long enough that a test sees real
/// intervals, short enough that a transition arrives without a five-second wait.
fn fast_hub(home: &Path) -> Hub {
    Hub::start_with(
        home,
        &[
            ("RADIAN_HUB_EVENTS_POLL_SECS", "1"),
            ("RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS", "1"),
        ],
    )
}

/// A plan with one task in `review.awaiting_human`, ready for a CLI reject.
fn plan_with_task_in_review(store: &Store, slug: &str) -> (String, String) {
    let (plan, task) = add_task(store, slug, "the event task");
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "awaiting_human".to_owned();
            task.attempt = 1;
            Ok(())
        })
        .expect("the task moves to review");
    (plan, task)
}

/// [`plan_with_task_in_review`]'s task id.
fn task_in_review(store: &Store, slug: &str) -> String {
    plan_with_task_in_review(store, slug).1
}

/// The owner's confirm of a draft plan through the CLI: a plan `status` event.
fn confirm_plan(repo: &Repo, plan: &str) {
    let output = repo.radian(&["arc", "confirm", plan]);
    assert_eq!(output.code, 0, "confirm {plan}: {}", output.stderr);
}

/// The owner's reject through the CLI: `review.awaiting_human` -> `review.rework_pending`, a
/// transition the channel sends.
fn reject(repo: &Repo, task: &str) {
    let output = repo.radian(&["arc", "reject", task, "--note", "the event note"]);
    assert_eq!(output.code, 0, "reject {task}: {}", output.stderr);
}

/// The owner's cancel through the CLI: `review.rework_pending` -> `failed.cancelled`, a
/// transition the channel sends.
fn cancel(repo: &Repo, task: &str) {
    let output = repo.radian(&["arc", "cancel", task]);
    assert_eq!(output.code, 0, "cancel {task}: {}", output.stderr);
}

/// A task's `state.phase` in a `list --json` array, found by id at any depth.
fn list_phase(list: &Value, task: &str) -> Option<String> {
    match list {
        Value::Array(items) => items.iter().find_map(|item| list_phase(item, task)),
        Value::Object(map) => {
            if map.get("id").and_then(Value::as_str) == Some(task) {
                let state = map.get("state").and_then(Value::as_str)?;
                let phase = map.get("phase").and_then(Value::as_str)?;
                return Some(format!("{state}.{phase}"));
            }
            map.values().find_map(|value| list_phase(value, task))
        }
        _ => None,
    }
}

fn task_event(repo: &str, task: &str, previous: &str, value: &str) -> Value {
    serde_json::json!({
        "repo": repo,
        "kind": "task",
        "id": task,
        "field": "state",
        "previous": previous,
        "value": value,
    })
}

fn plan_event(repo: &str, plan: &str, field: &str, previous: &str, value: &str) -> Value {
    serde_json::json!({
        "repo": repo,
        "kind": "plan",
        "id": plan,
        "field": field,
        "previous": previous,
        "value": value,
    })
}

// ---- edge case 1 -------------------------------------------------------------------------

#[test]
fn one_transition_in_each_repository_arrives_with_its_repository_and_the_channels_fields() {
    let a = Repo::new("hub-events-one-a");
    let b = Repo::new("hub-events-one-b");
    let task_a = task_in_review(&arc_store(&a), "one");
    let (plan_b, _task_b) = plan_with_task_in_review(&arc_store(&b), "one");
    let home = hub_home(&a);
    register(&home, "a", &a);
    register(&home, "b", &b);
    let hub = fast_hub(&home);
    let mut stream = EventStream::open(&hub);
    stream.keep_alive();

    reject(&a, &task_a);
    confirm_plan(&b, &plan_b);

    assert_eq!(
        stream.event(),
        task_event(
            "a",
            &task_a,
            "review.awaiting_human",
            "review.rework_pending"
        )
    );
    assert_eq!(
        stream.event(),
        plan_event("b", &plan_b, "status", "draft", "confirmed")
    );
}

// ---- edge case 2 -------------------------------------------------------------------------

#[test]
fn a_transition_the_channel_does_not_send_produces_no_event() {
    let a = Repo::new("hub-events-quiet");
    let store = arc_store(&a);
    let task = task_in_review(&store, "quiet");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);
    let mut stream = EventStream::open(&hub);
    stream.keep_alive();

    // `doing.running` -> `review.verifying` is outside the closed list. Both writes are the
    // same store transaction a gate's progress uses; the wait lets the poller observe the
    // running phase itself, not just its departure.
    store
        .change_task(&task, |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("the task runs");
    stream.keep_alives(2);
    store
        .change_task(&task, |task| {
            task.state = TaskState::Review;
            task.phase = "verifying".to_owned();
            Ok(())
        })
        .expect("the task verifies");

    // Three keep-alives cover more than one poll; an event would have been a line of its own.
    stream.keep_alives(3);
}

// ---- edge case 3 -------------------------------------------------------------------------

#[test]
fn a_client_that_connects_after_a_transition_gets_no_replay_and_the_lists_show_it() {
    let a = Repo::new("hub-events-replay");
    let store = arc_store(&a);
    let task = task_in_review(&store, "replay");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);

    // The transition happens before any client connects.
    reject(&a, &task);

    let mut stream = EventStream::open(&hub);
    // No replay: the baseline is the already-rejected state, so only keep-alives arrive.
    stream.keep_alives(2);

    // The lists show the transition the stream did not send.
    let list = hub.authorized("GET", "/v1/repos/a/items").json();
    assert_eq!(
        list_phase(&list, &task).as_deref(),
        Some("review.rework_pending")
    );

    // The first event is the next transition, not the one before the connection.
    cancel(&a, &task);
    assert_eq!(
        stream.event(),
        task_event("a", &task, "review.rework_pending", "failed.cancelled")
    );
}

// ---- edge case 4 -------------------------------------------------------------------------

#[test]
fn two_connected_clients_each_receive_the_same_event_once() {
    let a = Repo::new("hub-events-two-clients");
    let store = arc_store(&a);
    let task = task_in_review(&store, "two-clients");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);
    let mut one = EventStream::open(&hub);
    let mut two = EventStream::open(&hub);
    one.keep_alive();
    two.keep_alive();

    reject(&a, &task);

    let expected = task_event("a", &task, "review.awaiting_human", "review.rework_pending");
    assert_eq!(one.event(), expected);
    assert_eq!(two.event(), expected);
    // Each stream sends it once: the next line each holds is a keep-alive.
    one.keep_alive();
    two.keep_alive();
}

// ---- edge case 5 -------------------------------------------------------------------------

#[test]
fn an_event_stream_without_a_token_is_401_and_no_stream() {
    let a = Repo::new("hub-events-token");
    arc_store(&a);
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);

    let response = hub.request("GET", "/v1/events", None);
    assert_eq!(response.status, 401);
    assert_eq!(response.body, b"{}".to_vec());
}

// ---- edge case 6 -------------------------------------------------------------------------

#[test]
fn an_idle_stream_sends_a_keep_alive_comment() {
    let a = Repo::new("hub-events-keep-alive");
    let store = arc_store(&a);
    let _task = task_in_review(&store, "keep-alive");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);
    let mut stream = EventStream::open(&hub);

    stream.keep_alive();
}

// ---- the stream's first body bytes --------------------------------------------------------

/// `GET /v1/events` writes its `: open` comment at once. The keep-alive here is a minute away,
/// so a first byte within a second proves it is the open comment and not a keep-alive; a client
/// such as `URLSession.bytes(for:)`, which returns only at the first body byte, no longer waits.
#[test]
fn an_event_stream_opens_with_a_comment_line_at_once() {
    let a = Repo::new("hub-events-open");
    let store = arc_store(&a);
    let _task = task_in_review(&store, "open");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start_with(&home, &[("RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS", "60")]);

    let started = Instant::now();
    let mut stream = EventStream::open(&hub);
    assert_eq!(stream.line().as_deref(), Some(": open"));
    assert_eq!(stream.line().as_deref(), Some(""));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the open comment is the first body byte, not the first keep-alive: {:?}",
        started.elapsed()
    );
}

/// A transition that lands as the stream opens is still preceded by `: open`: the first body
/// bytes never depend on what the first poll has to read. The first poll is held back by a
/// test-only delay, so an `: open` written after it would arrive late; a transition raised
/// during that wait belongs to the first poll's baseline and produces no event at all.
#[test]
fn the_open_comment_precedes_a_transition_that_lands_as_the_stream_opens() {
    let a = Repo::new("hub-events-open-event");
    let store = arc_store(&a);
    let task = task_in_review(&store, "open-event");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = Hub::start_with(
        &home,
        &[
            ("RADIAN_HUB_EVENTS_POLL_SECS", "1"),
            ("RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS", "1"),
            ("RADIAN_HUB_EVENTS_FIRST_POLL_DELAY_MS", "2000"),
        ],
    );
    // The stream's first poll is still two seconds away: `: open` is the first body bytes
    // anyway, so a first byte written after that poll cannot pass this deadline.
    let mut stream = EventStream::open(&hub);
    let started = Instant::now();
    assert_eq!(stream.line().as_deref(), Some(": open"));
    assert_eq!(stream.line().as_deref(), Some(""));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the open comment precedes the held first poll: {:?}",
        started.elapsed()
    );

    // This transition lands while the first poll is still held, so that poll takes it as its
    // baseline and sends nothing for it; the keep-alive proves the poll has run.
    reject(&a, &task);
    stream.keep_alive();

    cancel(&a, &task);
    assert_eq!(
        stream.event(),
        task_event("a", &task, "review.rework_pending", "failed.cancelled")
    );
}

/// A client that reads the headers and then goes away leaves the hub serving: the dead stream's
/// writer ends, and the transition after it still reaches a live subscriber.
#[test]
fn a_client_that_disconnects_right_after_the_headers_leaves_the_hub_serving() {
    let a = Repo::new("hub-events-disconnect");
    let store = arc_store(&a);
    let task = task_in_review(&store, "disconnect");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);

    // Opening reads exactly the status and headers; dropping the connection then closes it
    // around the `: open` line, before anything is read from the body.
    drop(EventStream::open(&hub));

    let mut stream = EventStream::open(&hub);
    assert_eq!(stream.line().as_deref(), Some(": open"));
    stream.keep_alive();
    reject(&a, &task);
    assert_eq!(
        stream.event(),
        task_event("a", &task, "review.awaiting_human", "review.rework_pending")
    );
}

// ---- edge case 7 -------------------------------------------------------------------------

#[test]
fn a_stream_ends_when_the_hub_is_stopped() {
    let a = Repo::new("hub-events-sigterm");
    let store = arc_store(&a);
    let _task = task_in_review(&store, "sigterm");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let mut hub = fast_hub(&home);
    let mut stream = EventStream::open(&hub);
    stream.keep_alive();

    hub.terminate();

    // The client's read reaches the end of the body without a protocol error; a last
    // keep-alive that was already in flight is fine.
    while let Some(line) = stream.line() {
        assert!(
            !line.starts_with("event: transition"),
            "no event after the stop: {line:?}"
        );
    }
}

// ---- edge case 9 -------------------------------------------------------------------------

#[test]
fn a_repository_added_while_a_client_is_connected_sends_on_the_same_stream() {
    let a = Repo::new("hub-events-late-a");
    let b = Repo::new("hub-events-late-b");
    arc_store(&a);
    let task_b = task_in_review(&arc_store(&b), "late");
    let home = hub_home(&a);
    register(&home, "a", &a);
    let hub = fast_hub(&home);
    let mut stream = EventStream::open(&hub);
    stream.keep_alive();

    register(&home, "b", &b);
    // Three intervals is more than enough for the stream to re-read the registry and take
    // b's baseline; the keep-alives prove the stream is still running.
    stream.keep_alives(3);

    reject(&b, &task_b);
    let event = stream.event();
    assert_eq!(event["repo"], "b");
    assert_eq!(
        event,
        task_event(
            "b",
            &task_b,
            "review.awaiting_human",
            "review.rework_pending"
        )
    );
}

// ---- `[hub] bind` (ADR 0048 §16) ---------------------------------------------------------

/// A non-loopback IPv4 address this machine holds, found without sending a packet: a connected
/// UDP socket has no traffic, and its local address is the interface the route would use.
fn non_loopback_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// Whether this machine has an IPv6 loopback to bind `::1` on.
fn has_ipv6_loopback() -> bool {
    TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// Waits until `hub`'s stderr contains `needle`, and returns it. The warning follows the first
/// stdout line, so a test that read the line may need a moment.
fn wait_for_stderr(hub: &Hub, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stderr = hub.stderr();
        if stderr.contains(needle) {
            return stderr;
        }
        assert!(
            Instant::now() < deadline,
            "the hub stderr never contained {needle:?}: {stderr}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The default configuration listens on `127.0.0.1` and prints no warning.
#[test]
fn the_default_bind_is_loopback_and_prints_no_warning() {
    let a = Repo::new("hub-bind-default");
    let home = hub_home(&a);
    let hub = Hub::start(&home);
    assert_eq!(hub.address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(hub.authorized("GET", "/v1/version").status, 200);
    let stderr = hub.stderr();
    assert!(
        !stderr.to_lowercase().contains("warning"),
        "the default bind prints no warning: {stderr}"
    );
    assert!(
        !stderr.contains("unencrypted"),
        "the default bind prints no warning: {stderr}"
    );
}

/// `hub.bind = "0.0.0.0"` answers `/v1/version` on loopback and, when this machine has a
/// non-loopback IPv4 address, on that address too; stderr has the warning exactly once.
#[test]
fn a_wildcard_bind_answers_on_loopback_and_a_local_address_with_one_warning() {
    let a = Repo::new("hub-bind-wildcard");
    let home = hub_home(&a);
    let hub = Hub::start_with_config(&home, "schema_version = 1\n[hub]\nbind = \"0.0.0.0\"\n");
    assert_eq!(hub.address.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    assert_eq!(
        http_at(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            hub.port,
            "GET",
            "/v1/version",
            Some(&format!("Bearer {}", hub.token())),
            None,
        )
        .status,
        200,
        "the wildcard bind answers on loopback"
    );
    if let Some(ip) = non_loopback_ipv4() {
        assert_eq!(
            http_at(
                IpAddr::V4(ip),
                hub.port,
                "GET",
                "/v1/version",
                Some(&format!("Bearer {}", hub.token())),
                None,
            )
            .status,
            200,
            "the wildcard bind answers on {ip}"
        );
    }
    let stderr = wait_for_stderr(&hub, "unencrypted");
    assert_eq!(
        stderr.matches("unencrypted").count(),
        1,
        "the warning appears exactly once: {stderr}"
    );
}

/// `hub.bind = "::1"` prints the bracketed IPv6 address as its first stdout line.
#[test]
fn an_ipv6_loopback_bind_prints_the_bracketed_address() {
    if !has_ipv6_loopback() {
        return;
    }
    let a = Repo::new("hub-bind-ipv6");
    let home = hub_home(&a);
    let hub = Hub::start_with_config(&home, "schema_version = 1\n[hub]\nbind = \"::1\"\n");
    assert_eq!(hub.address.ip(), IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert!(
        hub.address.to_string().starts_with("[::1]:"),
        "the startup address is bracketed: {}",
        hub.address
    );
    assert_eq!(
        http_at(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            hub.port,
            "GET",
            "/v1/version",
            Some(&format!("Bearer {}", hub.token())),
            None,
        )
        .status,
        200,
        "the IPv6 loopback bind answers"
    );
    assert!(
        !hub.stderr().contains("unencrypted"),
        "a loopback bind prints no warning: {}",
        hub.stderr()
    );
}

/// A host name, an empty string and an address with a port are configuration errors naming
/// `hub.bind`; nothing binds and nothing is printed on stdout.
#[test]
fn a_bind_that_is_not_an_ip_address_is_a_configuration_error_and_nothing_binds() {
    let a = Repo::new("hub-bind-invalid");
    let home = hub_home(&a);
    for value in ["localhost", "", "127.0.0.1:7341"] {
        let config = home.join("bad-bind.toml");
        fs::write(
            &config,
            format!("schema_version = 1\n[hub]\nbind = \"{value}\"\n"),
        )
        .expect("the bad configuration writes");
        let output = radian(
            &home,
            &[
                "--config",
                config.to_str().expect("the path is UTF-8"),
                "hub",
                "--port",
                "0",
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{value:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("hub.bind"), "{value:?}: {stderr}");
        assert!(stderr.contains("an IP address"), "{value:?}: {stderr}");
        assert!(
            output.stdout.is_empty(),
            "{value:?}: nothing binds, so nothing is on stdout: {output:?}"
        );
    }
}

/// A non-loopback bind does not turn the token off: a request without it is `401`.
#[test]
fn a_non_loopback_bind_still_requires_the_bearer_token() {
    let a = Repo::new("hub-bind-token");
    let home = hub_home(&a);
    let hub = Hub::start_with_config(&home, "schema_version = 1\n[hub]\nbind = \"0.0.0.0\"\n");
    let response = http_at(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        hub.port,
        "GET",
        "/v1/version",
        None,
        None,
    );
    assert_eq!(response.status, 401);
    assert_eq!(response.json(), serde_json::json!({}));
}

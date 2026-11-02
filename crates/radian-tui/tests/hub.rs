//! `radian hub` through the real binary: `add`, `list`, `--json`, `remove`, the exit codes,
//! and the bare command that serves the HTTP API and runs the scheduling loop.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-hub-cli-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// A directory that looks like an arc repository: it has `.radian/arc/`.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.path(name);
        fs::create_dir_all(repo.join(".radian/arc")).expect("the arc directory is creatable");
        repo
    }

    fn registry(&self) -> PathBuf {
        self.home().join("hub/repos.toml")
    }

    /// Writes `$RADIAN_HOME/config/config.toml`: the file `[hub] bind` is read from.
    fn config(&self, text: &str) {
        let dir = self.home().join("config");
        fs::create_dir_all(&dir).expect("the config directory is creatable");
        fs::write(dir.join("config.toml"), text).expect("the config is writable");
    }

    fn run(&self, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_radian"))
            .current_dir(&self.dir)
            .env("RADIAN_HOME", self.home())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("radian runs");
        Run {
            code: output.status.code(),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// One finished `radian hub` run.
#[derive(Debug)]
struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("the path resolves")
        .display()
        .to_string()
}

#[test]
fn add_and_list_print_the_registered_repository_in_text_and_json() {
    let scratch = Scratch::new("add-list");
    let repo = scratch.repo("repo");

    let added = scratch.run(&["hub", "add", repo.to_str().unwrap()]);
    assert_eq!(added.code, Some(0), "{added:?}");
    let written = fs::read_to_string(scratch.registry()).expect("the registry is written");
    assert!(written.contains("name = \"repo\""), "{written}");
    assert!(
        written.contains(&format!("path = \"{}\"", canonical(&repo))),
        "{written}"
    );

    let listed = scratch.run(&["hub", "list"]);
    assert_eq!(listed.code, Some(0), "{listed:?}");
    assert_eq!(listed.out, format!("repo  {}\n", canonical(&repo)));

    let json = scratch.run(&["hub", "list", "--json"]);
    assert_eq!(json.code, Some(0), "{json:?}");
    let parsed: serde_json::Value = serde_json::from_str(&json.out).expect("json parses");
    assert_eq!(
        parsed,
        serde_json::json!([{ "name": "repo", "path": canonical(&repo) }])
    );
}

#[test]
fn add_refuses_a_directory_without_an_arc_store() {
    let scratch = Scratch::new("not-arc");
    let plain = scratch.path("plain");
    fs::create_dir_all(&plain).unwrap();

    let run = scratch.run(&["hub", "add", plain.to_str().unwrap()]);
    assert_eq!(run.code, Some(1), "{run:?}");
    assert!(
        run.err.contains(&canonical(&plain)),
        "the error names the path: {}",
        run.err
    );
    assert!(run.err.contains(".radian/arc"), "{}", run.err);
    assert!(
        !scratch.registry().exists(),
        "a refused add creates no registry"
    );
}

#[test]
fn remove_refuses_an_unknown_name_and_removes_only_the_named_entry() {
    let scratch = Scratch::new("remove");
    let one = scratch.repo("one");
    let two = scratch.repo("two");
    assert_eq!(
        scratch.run(&["hub", "add", one.to_str().unwrap()]).code,
        Some(0)
    );
    assert_eq!(
        scratch.run(&["hub", "add", two.to_str().unwrap()]).code,
        Some(0)
    );

    let unknown = scratch.run(&["hub", "remove", "missing"]);
    assert_eq!(unknown.code, Some(1), "{unknown:?}");
    assert!(unknown.err.contains("`missing`"), "{}", unknown.err);

    let removed = scratch.run(&["hub", "remove", "one"]);
    assert_eq!(removed.code, Some(0), "{removed:?}");
    let listed = scratch.run(&["hub", "list"]);
    assert_eq!(listed.out, format!("two  {}\n", canonical(&two)));
}

#[test]
fn hub_without_an_argument_serves_http_and_runs_the_loop() {
    let scratch = Scratch::new("bare");
    let mut child = Command::new(env!("CARGO_BIN_EXE_radian"))
        .current_dir(&scratch.dir)
        .env("RADIAN_HOME", scratch.home())
        .args(["hub", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("radian hub starts");
    let mut stdout = BufReader::new(child.stdout.take().expect("the hub has stdout"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("the first stdout line reads");
    assert!(
        line.starts_with("hub: listening on 127.0.0.1:"),
        "the first stdout line names the loopback listener: {line:?}"
    );

    // The same process runs the scheduling loop and holds `hub.lock` for its life.
    let lock = scratch.home().join("hub/hub.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lock.is_file() {
        assert!(Instant::now() < deadline, "the hub never wrote hub.lock");
        std::thread::sleep(Duration::from_millis(20));
    }

    let sent = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(sent.success(), "SIGTERM reaches the hub");
    let status = child.wait().expect("the hub exits");
    assert_eq!(status.code(), Some(0), "SIGTERM is not an error");
}

/// Runs `radian hub pair` under a real pty, so `is_terminal` is true, and returns what the
/// program wrote. The pty ends at EOF when the program exits.
fn pair_on_a_pty(scratch: &Scratch, args: &[&str]) -> String {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("a pty opens");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_radian"));
    for arg in args {
        command.arg(arg);
    }
    command.env("RADIAN_HOME", scratch.home().as_os_str());
    command.cwd(&scratch.dir);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("the hub pair command starts");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("the pty reads");
    let mut out = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => out.extend_from_slice(&buffer[..read]),
        }
    }
    let _ = child.wait();
    String::from_utf8_lossy(&out).into_owned()
}

fn contains_a_qr_block(text: &str) -> bool {
    text.contains('█') || text.contains('▀') || text.contains('▄')
}

#[test]
fn pair_off_a_terminal_prints_the_url_and_token_without_a_qr() {
    let scratch = Scratch::new("pair-pipe");
    let run = scratch.run(&["hub", "pair"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let token = fs::read_to_string(scratch.home().join("hub/token"))
        .expect("pair creates the token")
        .trim()
        .to_owned();
    assert_eq!(run.out, format!("http://127.0.0.1:7341\n{token}\n"));
    assert!(
        !contains_a_qr_block(&run.out),
        "a pipe gets no QR: {}",
        run.out
    );
}

#[test]
fn pair_with_a_url_prints_it_without_the_trailing_slash() {
    let scratch = Scratch::new("pair-url");
    let run = scratch.run(&["hub", "pair", "--url", "https://mac.example.ts.net/"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let token = fs::read_to_string(scratch.home().join("hub/token"))
        .expect("pair creates the token")
        .trim()
        .to_owned();
    let mut lines = run.out.lines();
    assert_eq!(
        lines.next(),
        Some("https://mac.example.ts.net"),
        "the trailing slash is dropped: {}",
        run.out
    );
    assert_eq!(lines.next(), Some(token.as_str()), "{}", run.out);
    assert_eq!(
        lines.next(),
        Some("other: http://127.0.0.1:7341"),
        "a URL that is not loopback lists the other addresses: {}",
        run.out
    );
}

#[test]
fn pair_refuses_a_url_that_is_not_an_absolute_http_url() {
    let scratch = Scratch::new("pair-url-bad");
    for url in [
        "ftp://x",
        "mac.local",
        "",
        "http://[not-an-ip]/",
        "https://example.test/%zz",
    ] {
        let run = scratch.run(&["hub", "pair", "--url", url]);
        assert_eq!(run.code, Some(2), "{url:?}: {run:?}");
        assert!(run.err.contains("--url"), "{url:?}: {}", run.err);
        assert!(
            run.err.contains("absolute http or https URL"),
            "{url:?} is refused by the URL check, not as an unknown flag: {}",
            run.err
        );
        assert!(run.out.is_empty(), "{url:?} prints no URL: {}", run.out);
    }
    assert!(
        !scratch.home().join("hub/token").exists(),
        "a refused --url creates no token"
    );
}

#[test]
fn pair_of_an_ipv6_loopback_bind_prints_the_loopback_url() {
    let scratch = Scratch::new("pair-ipv6-loopback");
    scratch.config("schema_version = 1\n[hub]\nbind = \"::1\"\n");
    let run = scratch.run(&["hub", "pair"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(
        run.out.lines().next(),
        Some("http://127.0.0.1:7341"),
        "any loopback bind prints the loopback URL a client can reach: {}",
        run.out
    );
}

#[test]
fn pair_prints_the_configured_bind_address() {
    let scratch = Scratch::new("pair-bind");
    scratch.config("schema_version = 1\n[hub]\nbind = \"192.0.2.10\"\n");
    let run = scratch.run(&["hub", "pair"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let mut lines = run.out.lines();
    assert_eq!(
        lines.next(),
        Some("http://192.0.2.10:7341"),
        "the bind's own address, not loopback: {}",
        run.out
    );
}

#[test]
fn pair_for_a_wildcard_bind_prints_a_lan_address_and_not_the_wildcard() {
    let scratch = Scratch::new("pair-wildcard");
    scratch.config("schema_version = 1\n[hub]\nbind = \"0.0.0.0\"\n");
    let run = scratch.run(&["hub", "pair"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let url = run.out.lines().next().expect("a URL line");
    assert!(url.starts_with("http://"), "{url}");
    assert!(url.ends_with(":7341"), "{url}");
    assert!(!url.contains("0.0.0.0"), "a phone cannot reach {url}");
    if url == "http://127.0.0.1:7341" {
        assert!(
            run.err.contains("no LAN address"),
            "a fallback says so on stderr: {}",
            run.err
        );
    }
}

#[test]
fn pair_on_a_terminal_prints_a_qr_block_and_no_qr_flag_prints_none() {
    let scratch = Scratch::new("pair-pty");
    let with_qr = pair_on_a_pty(&scratch, &["hub", "pair"]);
    assert!(with_qr.contains("http://127.0.0.1:7341"), "{with_qr}");
    assert!(
        contains_a_qr_block(&with_qr),
        "a terminal gets the QR half blocks: {with_qr}"
    );

    let without = pair_on_a_pty(&scratch, &["hub", "pair", "--no-qr"]);
    assert!(without.contains("http://127.0.0.1:7341"), "{without}");
    assert!(
        !contains_a_qr_block(&without),
        "--no-qr prints no QR: {without}"
    );
}

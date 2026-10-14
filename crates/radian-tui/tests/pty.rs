//! End-to-end through a pty: the bin is started under `portable-pty`, driven by bytes, and
//! its rendering is read from a `vt100` screen model. **The full-screen program is never run
//! in the test runner's own terminal.**
//!
//! **Timing rule**: `wait_for_screen(X)` only proves that `X` is on screen. An assertion about
//! some *other* text must either be drawn in the same frame as `X` (the approval box, the status
//! line and the input line are single widgets, so their text arrives together) or wait for that
//! text too. `pty_two_approvals_are_shown_in_order` flaked on exactly this: `b.txt` appears in
//! the transcript's tool-call line a frame before the second approval box is drawn.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_provider_anthropic::testing as anth;
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::json;
use vt100::Parser;

/// A unique temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-tui-pty-{}-{label}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Kill the app, not just the shell that started it. `portable-pty` calls `setsid()`,
        // so the shell is a process-group leader and the app it starts is in the same group:
        // a group kill reaps both. Killing only the shell (what `child.kill()` does) left the
        // app orphaned once the pty master closed -- that is how 16 `radian-tui` processes
        // survived their tests at 15-25% CPU.
        if let Some(pid) = self.child.process_id() {
            // SAFETY: SIGKILL to a process group this test owns.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills a directly-spawned child on drop, so a panicking test cannot leave the app behind
/// (the `Pty` harness does the same for its process group).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Writes `credentials/auth.json` (0600) under `root` for the key env vars in `envs` (ADR
/// 0026 D2: the key comes only from `auth.json`). `ANTHROPIC_API_KEY` maps to the `anthropic`
/// descriptor; `OPENAI_API_KEY` and any other `*KEY` name to the default `openai-compatible`.
fn write_auth_json(root: &Path, envs: &[(&str, &str)]) {
    let base_url = envs
        .iter()
        .find(|(name, _)| *name == "RADIAN_BASE_URL")
        .map(|(_, value)| *value);
    let mut entries = serde_json::Map::new();
    for (name, value) in envs {
        let provider = if *name == "ANTHROPIC_API_KEY" {
            "anthropic"
        } else if *name == "OPENCODE_GO_API_KEY" {
            "opencode-go"
        } else if name.ends_with("KEY") {
            "openai-compatible"
        } else {
            continue;
        };
        let mut entry = serde_json::json!({"type": "api_key", "key": value});
        if let Some(url) = base_url {
            entry["env"] = serde_json::json!({ "RADIAN_BASE_URL": url });
        }
        entries.insert(provider.to_string(), entry);
    }
    if entries.is_empty() {
        return;
    }
    let dir = root.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.join("auth.json");
    std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
/// Spawns the binary directly on a plain pty: **no controlling terminal** (so closing the
/// master is a pure EOF, no SIGHUP) and `FD_CLOEXEC` on the master (without it the child
/// inherits the master and the pty never sees EOF). `home` is a private HOME the caller keeps
/// alive. Returns the child (killed on drop) and the master for reading/closing.
fn spawn_direct(args: &[&str], home: &std::path::Path) -> (KillOnDrop, std::fs::File) {
    spawn_direct_in(args, home, None)
}

/// [`spawn_direct`], with the child's working directory set: the default-workspace tests need a
/// session started *from* a particular directory.
fn spawn_direct_in(
    args: &[&str],
    home: &std::path::Path,
    cwd: Option<&std::path::Path>,
) -> (KillOnDrop, std::fs::File) {
    let (child, master, _slave) = spawn_direct_keeping_slave(args, home, cwd);
    (child, master)
}

/// Like [`spawn_direct`], and also keeps a `dup` of the pty **slave**: the child's stdout is
/// that same open file description (a `dup` shares it; a fresh `open` would not), so a test can
/// inspect the status flags the child set on its stdout after the child is gone.
fn spawn_direct_keeping_slave(
    args: &[&str],
    home: &std::path::Path,
    cwd: Option<&std::path::Path>,
) -> (KillOnDrop, std::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    size.ws_row = 24;
    size.ws_col = 100;
    // SAFETY: `openpty` writes the two fds and reads the null termios/name pointers.
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    // SAFETY: `master_fd` is a valid fd; F_SETFD is async-signal-safe.
    unsafe { libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: the fds are fresh from `openpty`; `OwnedFd` closes them.
    let (master, slave) = unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(master_fd),
            std::os::fd::OwnedFd::from_raw_fd(slave_fd),
        )
    };

    let mut cmd = std::process::Command::new(BIN);
    cmd.args(args);
    cmd.env_clear();
    for name in ["PATH", "TMPDIR", "LANG"] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("HOME", home.as_os_str());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json");
    let keep_slave = slave.try_clone().unwrap();
    cmd.stdin(std::process::Stdio::from(slave.try_clone().unwrap()));
    cmd.stdout(std::process::Stdio::from(slave.try_clone().unwrap()));
    cmd.stderr(std::process::Stdio::from(slave));
    let child = KillOnDrop(cmd.spawn().unwrap());
    drop(cmd);
    let master = std::fs::File::from(master);
    // The first frame is larger than the pty output buffer, so the app blocks in `draw` until
    // the parent drains it; a non-blocking master lets the tests drain without a reader thread
    // (and still lets them close the only master fd).
    // SAFETY: `master` is a valid fd; F_SETFL is async-signal-safe.
    unsafe {
        use std::os::fd::AsRawFd;
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    (child, master, keep_slave)
}

const BIN: &str = env!("CARGO_BIN_EXE_radian");

/// The pty tests spawn real processes and mock servers; run them one at a time so the
/// whole workspace staying busy cannot make one of them flake.
static PTY_LOCK: Mutex<()> = Mutex::new(());

fn pty_guard() -> std::sync::MutexGuard<'static, ()> {
    PTY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Pty {
    writer: Box<dyn Write + Send>,
    bytes: Arc<Mutex<Vec<u8>>>,
    parser: Parser,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    processed: usize,
    /// A private `HOME` for the app: its catalog cache lives here, never in the user's real
    /// cache. Held only so the directory is removed when the pty is dropped.
    #[allow(dead_code)]
    home: TempDir,
}

impl Pty {
    /// Spawns the bin under `sh -c '"$@"; echo EXITED:$?; stty -a'` so the same pty reports
    /// the exit code and the tty state after the app exits. `Drop` kills the whole process
    /// group (the shell and the app), never just the shell.
    fn spawn(args: &[&str], rows: u16, cols: u16) -> Self {
        Self::spawn_with_env(args, &[], rows, cols)
    }

    /// Like [`Pty::spawn`], with extra environment for the app (e.g. the sandbox probe seam).
    fn spawn_with_env(args: &[&str], envs: &[(&str, &str)], rows: u16, cols: u16) -> Self {
        Self::spawn_full(args, envs, None, rows, cols)
    }

    /// Like [`Pty::spawn_with_env`], and writes `config` to `<RADIAN_HOME>/config/config.toml`
    /// so the app can start with no arguments (ADR 0026 slice 0b).
    fn spawn_with_config(
        args: &[&str],
        envs: &[(&str, &str)],
        config: &str,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self::spawn_full(args, envs, Some(config), rows, cols)
    }

    fn spawn_full(
        args: &[&str],
        envs: &[(&str, &str)],
        config: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(r#""$@"; echo "EXITED:$?"; stty -a"#);
        cmd.arg("sh");
        cmd.arg(BIN);
        for arg in args {
            cmd.arg(arg);
        }
        // Keep the environment small and deterministic; the mock server needs no key.
        cmd.env_clear();
        for name in ["PATH", "HOME", "TMPDIR", "LANG", "TERM"] {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        // Tests must not read the user's catalog cache or reach the real models.dev. A fresh
        // `HOME` isolates the cache; the catalog URL points at a closed local port, so a
        // lookup fails fast and the window is unknown. A test that wants the catalog
        // overrides both through `envs`.
        let home = TempDir::new("home");
        cmd.env("HOME", home.path().as_os_str());
        // The credential file lives under the same root the app will use: `RADIAN_HOME` if
        // the test set one, else the `HOME` it set, else the fresh `HOME` above.
        let auth_root = envs
            .iter()
            .find(|(name, _)| *name == "RADIAN_HOME")
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| {
                envs.iter()
                    .find(|(name, _)| *name == "HOME")
                    .map(|(_, value)| PathBuf::from(value).join(".radian"))
            })
            .unwrap_or_else(|| home.path().join(".radian"));
        write_auth_json(&auth_root, envs);
        if let Some(config) = config {
            let dir = auth_root.join("config");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), config).unwrap();
        }
        // `cmd.env_clear()` above is what makes this hermetic: neither `RADIAN_HOME` nor
        // `RADIAN_CONFIG` reaches the child, so a config can only come from this temp `HOME`.
        cmd.env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json");
        for (name, value) in envs {
            cmd.env(name, value);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&bytes);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            writer,
            bytes,
            parser: Parser::new(rows, cols, 5000),
            child,
            processed: 0,
            home,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn pump(&mut self) {
        let snapshot = self.bytes.lock().unwrap().clone();
        if snapshot.len() > self.processed {
            self.parser.process(&snapshot[self.processed..]);
            self.processed = snapshot.len();
        }
    }

    /// Waits until `needle` shows on the screen. `pump` is a state check, not a sleep used
    /// for synchronization; the deadline only bounds a failure.
    fn wait_for_screen(&mut self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.pump();
            if self.parser.screen().contents().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until `predicate` holds for the current screen contents. The predicate is the
    /// synchronization; the deadline only bounds a failure.
    fn wait_until(&mut self, secs: u64, predicate: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if predicate(&self.screen()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits without killing: on timeout the caller decides (the `Drop` impl still kills).
    fn wait_for_exit(&mut self, secs: u64) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.pump();
            if let Ok(Some(status)) = self.child.try_wait() {
                // Let the shell's trailing `stty -a` output arrive.
                std::thread::sleep(Duration::from_millis(150));
                self.pump();
                return Some(status.exit_code());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ctrl-C cancels a running turn and quits when idle, so press it until the app exits.
    fn quit(&mut self) {
        for _ in 0..4 {
            self.send(&[0x03]);
            if self.wait_for_exit(3).is_some() {
                return;
            }
        }
        panic!("the app did not exit after repeated Ctrl-C");
    }

    fn raw(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// Waits until `needle` appears in the raw output. Control sequences (mouse reporting)
    /// never reach the `vt100` screen, so some assertions read the bytes directly.
    fn wait_for_raw(&self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if String::from_utf8_lossy(&self.raw()).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The app's exit code, parsed from the shell wrapper's `EXITED:<n>` (the shell's own
    /// status is `stty`'s, not the app's).
    fn app_exit_code(&self) -> Option<i32> {
        let text = String::from_utf8_lossy(&self.raw()).into_owned();
        let rest = text.rsplit_once("EXITED:")?.1;
        rest.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }

    /// Waits until `needle` is **gone** from the screen (a modal that was cleared).
    fn wait_until_gone(&mut self, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.pump();
            if !self.parser.screen().contents().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn screen(&mut self) -> String {
        self.pump();
        self.parser.screen().contents()
    }

    /// The shell's pid, which is also the process group id: `portable-pty` calls `setsid()`
    /// and the shell has no job control, so the app it starts is in the same group. A group
    /// with no members therefore means the app is gone.
    fn process_group(&self) -> Option<i32> {
        self.child.process_id().map(|pid| pid as i32)
    }
}

/// One tool call, then (for the next request) the given text.
fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

/// The MCP fixture, built next to the TUI binary. `cargo test -p radian-tui` alone does not
/// necessarily build it, so the failure says what to run.
fn fixture_bin() -> PathBuf {
    let path = Path::new(BIN).parent().unwrap().join("mcp-fixture");
    assert!(
        path.exists(),
        "mcp-fixture is not built; run: cargo build -p agent-extensions --bin mcp-fixture"
    );
    path
}

/// An `--mcp-config` pointing at the fixture, logging every received method to `log`.
fn mcp_config(dir: &Path, log: &Path) -> PathBuf {
    let server = json!({
        "id": "fx",
        "command": fixture_bin(),
        "env": {"FIXTURE_LOG": log},
        "call_timeout_secs": 30,
    });
    let path = dir.join("mcp.json");
    std::fs::write(&path, json!({"servers": [server]}).to_string()).unwrap();
    path
}

/// One turn: send `hello`, wait for the mock reply, then quit with Ctrl-C.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the four pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_end_to_end_conversation_and_clean_exit() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("ASSISTANT-REPLY-OK")]).await;
    let dir = TempDir::new("e2e");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "e2e",
        ],
        24,
        100,
    );

    assert!(
        pty.wait_for_screen("message", 10),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"hello from the test\r");
    assert!(
        pty.wait_for_screen("ASSISTANT-REPLY-OK", 15),
        "reply not rendered:\n{}",
        pty.screen()
    );

    // Ctrl-C cancels if the turn is somehow still running, then quits.
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit code");

    let raw = pty.raw();
    let raw_text = String::from_utf8_lossy(&raw);
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(
        raw_text.contains("\u{1b}[?2004l"),
        "bracketed paste disabled"
    );
    assert!(raw_text.contains("\u{1b}[?25h"), "cursor restored");
    // The fix this slice makes: the app's last output is a newline after leaving the
    // alternate screen, so the shell does not print its `%` marker.
    // The app writes `\r\n` after leaving the alternate screen; with the pty back in
    // cooked mode ONLCR may turn the `\n` into `\r\n`, so a carriage return right after
    // the leave sequence is the stable marker.
    assert!(
        raw_text.contains("\u{1b}[?1049l\r"),
        "no newline written after restoring the screen"
    );
    // Same pty, after exit: the shell reports cooked mode.
    assert!(
        raw_text.contains("icanon"),
        "stty output missing:\n{raw_text}"
    );
    assert!(
        !raw_text.contains("-icanon"),
        "raw mode was not restored:\n{raw_text}"
    );
    // The session file was written under the temp dir, named by the session id.
    assert!(dir.path().join("e2e.jsonl").exists());
}

/// ADR 0026 slice 0b acceptance: with `model.provider` and `model.model` in the config and a
/// matching `auth.json` entry, `radian` starts with **no arguments** from the project directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_launches_with_no_arguments_from_the_config_and_auth_json() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("NO-ARGS-REPLY-OK")]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty =
        Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-no-args")], &config, 24, 100);
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("NO-ARGS-REPLY-OK", 20),
        "the reply is not on screen:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-no-args")
    );
}

/// ADR 0025 slice 3 acceptance: `/skill:<name> [args]` puts the skill's body in one user
/// message, `/skills` lists the discovered skills, `/skill:nope` is a notice and not a message,
/// and `//skill:x` is an ordinary message. A `disable-model-invocation: true` skill is listed for
/// the user but never reaches the prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_skill_commands_expand_list_notice_and_escape() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        text_reply("SKILL-REPLY-OK"),
        text_reply("ESCAPE-REPLY-OK"),
    ])
    .await;
    let dir = TempDir::new("skill-session");
    let home = TempDir::new("skill-home");
    // A normal skill and one the model must not invoke.
    std::fs::create_dir_all(home.path().join("skills/greet")).unwrap();
    std::fs::write(
        home.path().join("skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: Say hello.\n---\nGREET-SKILL-BODY-MARKER\nRun scripts/x.sh relative to me.\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join("skills/secret")).unwrap();
    std::fs::write(
        home.path().join("skills/secret/SKILL.md"),
        "---\nname: secret\ndescription: SECRET-DESCRIPTION\ndisable-model-invocation: true\n---\nSECRET-BODY-MARKER\n",
    )
    .unwrap();

    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "skill",
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());

    // `/skill:greet hello 世界`: the body and the args are one user message.
    pty.send("/skill:greet hello 世界\r".as_bytes());
    assert!(
        pty.wait_for_screen("SKILL-REPLY-OK", 20),
        "the reply is missing:\n{}",
        pty.screen()
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    let body = requests[0].json();
    let messages = body["messages"].as_array().expect("messages is an array");
    let user = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(user.contains("<skill name=\"greet\""), "{user}");
    assert!(user.contains("GREET-SKILL-BODY-MARKER"), "{user}");
    assert!(user.contains("hello 世界"), "{user}");
    assert!(
        !user.contains("description: Say hello."),
        "the frontmatter is stripped: {user}"
    );
    // The disabled skill is not in the system prompt; the visible one is.
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default();
    assert!(system.contains("## Skills (ADR 0025)"), "{system}");
    assert!(system.contains("greet"), "{system}");
    assert!(!system.contains("secret"), "{system}");

    // The journal records what the model saw (ADR 0025 S3): the expanded body, not the command.
    // The expansion is the message itself, so there is no path where the model sees one text and
    // the journal another; this pins that.
    let journal = std::fs::read_to_string(dir.path().join("skill.jsonl")).unwrap();
    assert!(journal.contains("GREET-SKILL-BODY-MARKER"), "{journal}");

    // `/skills` lists both the visible and the model-invocation-disabled skill.
    pty.send("/skills\r".as_bytes());
    assert!(
        pty.wait_for_screen("greet: Say hello.", 10),
        "the list is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("secret: SECRET-DESCRIPTION (model-invocation off)", 10),
        "the disabled skill is not listed:\n{}",
        pty.screen()
    );

    // `/skill:nope` is a notice, not a message: no second request.
    pty.send("/skill:nope\r".as_bytes());
    assert!(
        pty.wait_for_screen("no skill named `nope`", 10),
        "the notice is missing:\n{}",
        pty.screen()
    );
    assert_eq!(
        server.requests().len(),
        1,
        "an unknown skill must not reach the model"
    );

    // `//skill:greet` is the escape: an ordinary message with one slash dropped.
    pty.send("//skill:greet\r".as_bytes());
    assert!(
        pty.wait_for_screen("ESCAPE-REPLY-OK", 20),
        "the escaped message got no reply:\n{}",
        pty.screen()
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the escaped message is a normal turn");
    let body = requests[1].json();
    let last_user = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        last_user, "/skill:greet",
        "the escape sends the text as typed"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit code");
}

/// The diagnostics directory the app will use when `HOME` is `home` (ADR 0020):
/// `<home>/.radian/cache/diagnostics`.
fn diagnostics_dir(home: &Path) -> PathBuf {
    radian_host::diagnostics_dir_for(&home.join(".radian"))
}

/// The panic hook restores the terminal before the panic message is printed, and the panic log
/// goes to the diagnostics directory, not next to the session (ADR 0013 decision 4).
#[test]
fn pty_panic_path_restores_the_terminal() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic");
    // A directory that does not exist yet, so the app creates it owner-only.
    let sessions = dir.path().join("sessions");
    let diagnostics = diagnostics_dir(dir.path());
    let home = dir.path().to_str().unwrap();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--panic-after-enter",
        ],
        &[("HOME", home), ("OPENAI_API_KEY", "sk-panic-secret")],
        20,
        80,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after panic"
    );
    assert_eq!(pty.app_exit_code(), Some(101), "panic must exit 101");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(
        raw_text.contains("\u{1b}[?2004l"),
        "bracketed paste disabled"
    );
    assert!(raw_text.contains("panicked"), "panic message printed");
    assert!(raw_text.contains("icanon"), "cooked mode restored");
    assert!(!raw_text.contains("-icanon"), "raw mode not restored");
    // The exit line names the panic log so the user can find it.
    assert!(
        raw_text.contains("panic log:"),
        "the exit line must name the panic log: {raw_text}"
    );

    // The diagnostics directory and the panic log are owner-only (ADR 0013).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&sessions), 0o700, "the session directory must be 0700");
        assert_eq!(
            mode(&diagnostics),
            0o700,
            "the diagnostics directory must be 0700"
        );
        let log = std::fs::read_dir(&diagnostics)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.to_string_lossy().ends_with(".panic.log"))
            .expect("a panic log was written");
        assert_eq!(mode(&log), 0o600, "the panic log must be 0600");
        // ADR 0013: the resolved key is redacted everywhere, including the panic log.
        assert!(
            !raw_text.contains("sk-panic-secret"),
            "the key reached the terminal: {raw_text}"
        );
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(
            !log_text.contains("sk-panic-secret"),
            "the key reached the panic log: {log_text}"
        );
        // Nothing was written next to the session.
        let beside = std::fs::read_dir(&sessions)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .any(|path| path.to_string_lossy().ends_with(".panic.log"));
        assert!(
            !beside,
            "the panic log must not live in the session directory"
        );
    }
}

/// A diagnostics write that cannot happen is one line on stderr, never a fault: the run
/// continues and a panic still exits 101 (ADR 0013 decision 4).
#[test]
fn pty_an_unwritable_diagnostics_directory_is_only_a_notice() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-unwritable");
    let home = dir.path();
    // Make the resolved diagnostics path a regular file: creating or writing under it fails.
    let diagnostics = diagnostics_dir(home);
    std::fs::create_dir_all(diagnostics.parent().unwrap()).unwrap();
    std::fs::write(&diagnostics, b"not a directory").unwrap();
    let sessions = home.join("sessions");
    let home = home.to_str().unwrap();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--panic-message",
            "boom",
        ],
        &[("HOME", home)],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the app did not exit");
    assert_eq!(
        pty.app_exit_code(),
        Some(101),
        "a diagnostics failure must not change the panic exit"
    );
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains("[diagnostics]"),
        "the failure is a one-line notice: {raw}"
    );
    assert!(
        raw.contains("panicked"),
        "the panic reason is still shown: {raw}"
    );
    // The blocking file is untouched.
    assert_eq!(std::fs::read(&diagnostics).unwrap(), b"not a directory");
}

/// Waits until `predicate` holds, bounded. The sleep is a poll interval, not the
/// synchronization itself.
fn gone_within(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group has a member.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

/// The panic path must not orphan a child the turn had started. The test child is put in its
/// own process group, so "no orphan" is asserted on the group as well as the pid.
#[test]
fn pty_panic_path_does_not_orphan_a_child() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-child");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--panic-with-child",
            "600",
        ],
        20,
        80,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after the panic"
    );
    assert_eq!(pty.app_exit_code(), Some(101), "panic must exit 101");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(raw_text.contains("icanon"), "cooked mode restored");

    let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
        .expect("the app wrote child.pid before panicking")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        gone_within(pid, Duration::from_secs(10)),
        "the child {pid} outlived the panic"
    );
    assert!(
        !group_alive(pid),
        "the child's process group {pid} still has members"
    );
}

/// `--resume` renders the stored history before the first frame. The session file is written
/// with the same storage API the app uses, so the test also pins the on-disk convention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_resume_renders_the_stored_history() {
    let _serial = pty_guard();
    let dir = TempDir::new("resume");
    let path = dir.path().join("resume-me.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "resume-me",
        radian_session::SessionCreatedData::new("/tmp", json!({})),
    )
    .expect("create the session file");
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("OLD-USER-TURN"),
        })
        .await
        .unwrap();
    journal
        .append(agent_core::JournalRecord::Message {
            index: 1,
            message: agent_core::Message::assistant("OLD-ASSISTANT-REPLY"),
        })
        .await
        .unwrap();
    drop(journal);

    let server = MockServer::start(vec![text_reply("new reply")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "resume-me",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("OLD-USER-TURN", 10),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("OLD-ASSISTANT-REPLY", 10),
        "the assistant reply is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("resumed session resume-me", 10),
        "the resume notice is missing:\n{}",
        pty.screen()
    );

    // The resumed session keeps appending to the same file.
    pty.send(b"continue\r");
    assert!(
        pty.wait_for_screen("new reply", 15),
        "the resumed session did not run a turn:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The approval dialog end to end: the tool runs only after the human answers, the prompt
/// shows the **resolved** path, and the decision is written to the session file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_allow_once_runs_the_tool() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let server = MockServer::start(vec![
        // A relative path: the prompt must show the resolved absolute one (the `..`
        // normalization itself is covered by the permission layer's own tests).
        tool_reply(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"approved"}"#,
        ),
        text_reply("WROTE-IT"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "approve",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        // Wide enough that a long macOS temp path is not wrapped in the approval box;
        // the resolved-path assertion below needs it on one line.
        140,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"please write\r");

    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval prompt:\n{}",
        pty.screen()
    );
    // The prompt shows the resolved path: the model asked for `out.txt`, the prompt names
    // the absolute file the write will touch.
    let prompt_screen = pty.screen();
    assert!(prompt_screen.contains("tool: write"), "{prompt_screen}");
    let resolved = std::fs::canonicalize(&ws).unwrap().join("out.txt");
    assert!(
        prompt_screen.contains(&resolved.display().to_string()),
        "the resolved path is missing from the prompt:\n{prompt_screen}"
    );
    // Nothing ran yet.
    assert!(!ws.join("out.txt").exists(), "the tool ran before approval");

    pty.send(b"y");
    assert!(
        pty.wait_for_screen("WROTE-IT", 20),
        "the approved call did not complete:\n{}",
        pty.screen()
    );
    assert!(
        ws.join("out.txt").exists(),
        "the approved write did not land"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    // Every decision is durable in the session file (ADR 0006 §5.9).
    let session = std::fs::read_to_string(sessions.join("approve.jsonl")).unwrap();
    assert!(session.contains("PermissionResolved"), "{session}");
    assert!(session.contains("\"approver\":\"user\""), "{session}");
    assert!(session.contains("\"decision\":\"allow\""), "{session}");
}

/// Denying feeds a denial result back to the model and never runs the tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_deny_is_fed_back_to_the_model() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve-deny");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"denied.txt","content":"no"}"#),
        text_reply("SAW-THE-DENIAL"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "approve-deny",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no prompt:\n{}",
        pty.screen()
    );
    pty.send(b"n");
    // The reply and the denial are two different events: wait for both, so a frame that shows
    // the reply before the tool result's line cannot make this flake.
    assert!(
        pty.wait_until(20, |screen| {
            screen.contains("SAW-THE-DENIAL") && screen.contains("denied")
        }),
        "the turn did not continue after the denial:\n{}",
        pty.screen()
    );
    assert!(!ws.join("denied.txt").exists(), "the denied tool ran");

    pty.quit();
    // The decision is durable and attributed to the user.
    let session = std::fs::read_to_string(sessions.join("approve-deny.jsonl")).unwrap();
    assert!(session.contains("\"decision\":\"deny\""), "{session}");
    assert!(session.contains("\"approver\":\"user\""), "{session}");
}

/// "Always this session" applies to the next matching call without a second prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_always_skips_the_next_prompt() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve-session");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"same.txt","content":"one"}"#),
        tool_reply("call_2", "write", r#"{"path":"same.txt","content":"two"}"#),
        text_reply("DONE-TWICE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "approve-session",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no first prompt:\n{}",
        pty.screen()
    );
    pty.send(b"a");
    // No further key press: if the second call asked again the turn would stall here.
    assert!(
        pty.wait_for_screen("DONE-TWICE", 20),
        "the session grant did not cover the second call:\n{}",
        pty.screen()
    );
    let written = std::fs::read_to_string(ws.join("same.txt")).unwrap();
    assert_eq!(written, "two", "the second write did not run");

    pty.quit();
}

/// Ctrl-C while the prompt is up cancels the turn: the prompt goes away, the tool never runs,
/// and the app is still usable (a second Ctrl-C quits).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_ctrl_c_while_waiting_for_approval_cancels_the_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("approve-cancel");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![tool_reply(
        "call_1",
        "write",
        r#"{"path":"never.txt","content":"x"}"#,
    )])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "approve-cancel",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no prompt:\n{}",
        pty.screen()
    );

    pty.send(&[0x03]); // Ctrl-C: cancel the turn, not answer the prompt
    assert!(
        pty.wait_for_screen("cancelled", 20),
        "the turn was not cancelled:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "the prompt outlived the cancelled turn:\n{}",
        pty.screen()
    );
    assert!(!ws.join("never.txt").exists(), "the denied tool ran");

    // The app is not hung: Ctrl-C again quits cleanly.
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0018: over the input budget `B` the request is **still sent** and the turn completes;
/// the status bar shows the projection over `B` and labels a local estimate honestly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_an_over_budget_request_is_still_sent() {
    let _serial = pty_guard();
    let dir = TempDir::new("budget-over");
    let server = MockServer::start(vec![text_reply("OVER-BUDGET-REPLY")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "budget-over",
            // ADR 0018: R = min(16384, C/4) = 250, so B = C - R = 750. The input is
            // sent as one bracketed paste: a raw burst of 4 000 keys makes the app redraw once
            // per key, which is O(n^2) and timed out on a slow Linux VM.
            "--context-window",
            "1000",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // ~1000 estimated tokens, over B = 750.
    pty.send(format!("\x1b[200~{}\x1b[201~\r", "x".repeat(4_000)).as_bytes());
    assert!(
        pty.wait_for_screen("OVER-BUDGET-REPLY", 20),
        "the over-budget request must still be sent:\n{}",
        pty.screen()
    );
    assert_eq!(server.requests().len(), 1);
    let screen = pty.screen();
    assert!(
        screen.contains("(estimated)"),
        "the projection is labelled as an estimate: {screen}"
    );
    assert!(
        screen.contains("/ 750"),
        "the status bar shows the projection over B: {screen}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Without `--context-window` the budget is off, and the status bar says so instead of
/// showing a guessed number (ADR 0015 C1's `Disabled` row).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_without_a_window_the_status_bar_says_the_budget_is_off() {
    let _serial = pty_guard();
    let dir = TempDir::new("budget-off");
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "budget-off",
        ],
        24,
        110,
    );
    assert!(
        pty.wait_for_screen("context budget off", 10),
        "the status bar does not say the budget is off:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("no window configured"),
        "{}",
        pty.screen()
    );
    pty.quit();
}

/// A failed sandbox probe is a refusal, never an unsandboxed run (ADR 0012 decision 2). The
/// seam is `RADIAN_SANDBOX_PROBE_FAIL=1`, which can only cause a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_failed_sandbox_probe_refuses_instead_of_running_unsandboxed() {
    let _serial = pty_guard();
    let dir = TempDir::new("probe-fail");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("SHOULD-NOT-RUN")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "probe-fail",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after the probe failed"
    );
    assert_eq!(pty.app_exit_code(), Some(1), "a refusal is a failure exit");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw_text.contains("sandbox backend is unavailable")
            || raw_text.contains("failed its probe"),
        "the refusal is not explained: {raw_text}"
    );
    assert!(
        !raw_text.contains("\u{1b}[?1049h"),
        "the app entered the alternate screen although it refused to run"
    );
    assert!(
        server.requests().is_empty(),
        "no request may be sent when the sandbox cannot be enforced"
    );
}

/// The shell approval shows the whole command, and a capped result keeps the truncation
/// marker visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_shell_approval_shows_the_command_and_the_truncation_marker() {
    let _serial = pty_guard();
    let dir = TempDir::new("shell");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The sessions directory is a sibling of the workspace, not its parent: the sandbox masks
    // the sessions directory (ADR 0020), and bwrap's `--tmpfs` mask would hide a workspace
    // nested inside it, so the shell could not even chdir in.
    let sessions = dir.path().join("sessions");
    // ~200 KiB of output, well past the 64 KiB per-stream default: the marker must show.
    let command = "i=0; while [ $i -lt 20000 ]; do echo filler-$i; i=$((i+1)); done";
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", &json!({"command": command}).to_string()),
        text_reply("SHELL-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "shell",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "a shell command must ask:\n{}",
        pty.screen()
    );
    // The prompt shows the whole command, not a truncated preview.
    let prompt_screen = pty.screen();
    assert!(prompt_screen.contains("command:"), "{prompt_screen}");
    assert!(
        prompt_screen.contains("while [ $i -lt 20000 ]"),
        "the full command is not shown:\n{prompt_screen}"
    );

    pty.send(b"y");
    assert!(
        pty.wait_for_screen("SHELL-DONE", 30),
        "the approved shell command did not finish:\n{}",
        pty.screen()
    );
    let screen = pty.screen();
    assert!(
        screen.contains("bytes discarded"),
        "the truncation marker must be visible:\n{screen}"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// A compound command is never matched by an allow rule, so it always asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_compound_shell_command_always_asks() {
    let _serial = pty_guard();
    let dir = TempDir::new("shell-compound");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    // The allowed command reads a file: the output must appear on screen, and it is not a
    // substring of the command text, so the assertion can only pass if the shell really ran.
    std::fs::write(ws.join("value.txt"), "ALLOWED-OUTPUT-42\n").unwrap();
    let allowed = format!("cat {}/value.txt", ws.display());
    let compound = "echo ok; echo two";
    let server = MockServer::start(vec![
        tool_reply("call_1", "shell", &json!({"command": allowed}).to_string()),
        text_reply("FIRST-DONE"),
        tool_reply("call_2", "shell", &json!({"command": compound}).to_string()),
        text_reply("SECOND-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "shell-compound",
            "--workspace",
            ws.to_str().unwrap(),
            "--allow-shell-command",
            &allowed,
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // The simple command is allowed by the rule: it runs with no key pressed, and its output
    // proves it actually ran.
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("ALLOWED-OUTPUT-42", 20),
        "the allowed command did not run:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("FIRST-DONE", 20),
        "the allowed command was blocked:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "an allowed simple command must not ask:\n{}",
        pty.screen()
    );

    // The compound one is not a simple command, so the allow rule cannot match it.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "a compound command must ask:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains(compound),
        "the prompt must show the whole compound command:\n{}",
        pty.screen()
    );
    pty.send(b"n");
    assert!(pty.wait_for_screen("SECOND-DONE", 20), "{}", pty.screen());

    pty.quit();
}

/// A panic inside the driver task is caught by the runtime, but the UI must notice that the
/// session is gone: leave in order, restore the terminal, and report the failure where the
/// user can see it (after the alternate screen is gone).
#[test]
fn pty_a_driver_panic_exits_orderly_and_restores_the_terminal() {
    let _serial = pty_guard();
    let dir = TempDir::new("driver-panic");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "driver-panic",
            "--panic-in-driver",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the TUI did not exit after the driver task panicked"
    );
    // Not 101: the process did not panic, its driver did. The app exits 1 with a message.
    assert_eq!(pty.app_exit_code(), Some(1), "an orderly failure exit");
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(
        raw_text.contains("\u{1b}[?2004l"),
        "bracketed paste disabled"
    );
    assert!(raw_text.contains("\u{1b}[?25h"), "cursor restored");
    assert!(raw_text.contains("icanon"), "cooked mode restored");
    assert!(!raw_text.contains("-icanon"), "raw mode not restored");
    // The panic is reported after the alternate screen is gone, so it is readable.
    let after_restore = raw_text
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    assert!(
        after_restore.contains("driver task ended unexpectedly")
            || after_restore.contains("driver task panic test"),
        "the failure was not reported on the normal screen: {after_restore}"
    );
}

/// A bracketed paste arrives whole and multi-line in the input box.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the four pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_multiline_paste_reaches_the_input_box() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("after-paste")]).await;
    let dir = TempDir::new("paste");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10));

    pty.send(b"\x1b[200~paste line one\npaste line two\npaste line three\x1b[201~");
    assert!(
        pty.wait_for_screen("paste line one", 10),
        "paste not shown:\n{}",
        pty.screen()
    );
    let screen = pty.screen();
    assert!(
        screen.contains("paste line three"),
        "all paste lines kept:\n{screen}"
    );
    assert!(
        !screen.contains("paste line one\npaste line two"),
        "the paste is in the input box, not the transcript"
    );

    pty.quit();
}

/// The second request must not carry the first response's reasoning (ADR 0014 R4), verified
/// on the real UI path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the four pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_reasoning_is_not_sent_back_to_the_provider() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(json!({"reasoning_content": "SECRET-REASONING-TEXT"}), None),
            chunk(json!({"content": "first answer"}), None),
            chunk(json!({}), Some("stop")),
        ]),
        text_reply("second answer"),
    ])
    .await;
    let dir = TempDir::new("reasoning");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10));

    pty.send(b"first\r");
    assert!(pty.wait_for_screen("first answer", 15), "{}", pty.screen());
    pty.send(b"second\r");
    assert!(pty.wait_for_screen("second answer", 15), "{}", pty.screen());

    let requests = server.requests();
    assert!(requests.len() >= 2, "two requests expected");
    let second = &requests[1].body;
    assert!(
        !second.contains("reasoning"),
        "reasoning leaked into the request: {second}"
    );
    // The assistant message itself is still in history.
    assert!(second.contains("first answer"), "{second}");

    pty.quit();
}

/// The TUI's `--header` is the same parser `agent-cli` uses: `{session_id}` expands to this
/// process's session id, so a routing header can name the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_header_value_expands_the_session_id() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-new");
    let server = MockServer::start(vec![text_reply("HEADER-OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "abc",
            "--header",
            "x-s={session_id}",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hi\r");
    assert!(
        pty.wait_for_screen("HEADER-OK", 15),
        "reply not rendered:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(
        server.requests()[0].header("x-s"),
        Some("abc"),
        "the placeholder must expand to the session id"
    );
}

/// A resumed session expands `{session_id}` to the id being resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_resumed_session_header_uses_the_resumed_id() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-resume");
    let server = MockServer::start(vec![text_reply("FIRST"), text_reply("SECOND")]).await;

    let mut first = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "abc",
        ],
        24,
        100,
    );
    assert!(first.wait_for_screen("message", 10), "{}", first.screen());
    first.send(b"one\r");
    assert!(first.wait_for_screen("FIRST", 15), "{}", first.screen());
    first.quit();
    assert_eq!(first.app_exit_code(), Some(0));

    let mut second = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "abc",
            "--header",
            "x-s={session_id}",
        ],
        24,
        100,
    );
    assert!(second.wait_for_screen("message", 10), "{}", second.screen());
    second.send(b"two\r");
    assert!(second.wait_for_screen("SECOND", 15), "{}", second.screen());
    second.quit();
    assert_eq!(second.app_exit_code(), Some(0));
    assert_eq!(
        server.requests()[1].header("x-s"),
        Some("abc"),
        "the resumed id must be the one expanded"
    );
}

/// `authorization` is refused (the key has one source), and the refusal never echoes the
/// value. The app refuses before entering the alternate screen, so it exits with an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_an_authorization_header_is_refused_without_echoing_its_value() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-auth");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "auth",
            "--header",
            "Authorization=Bearer sk-from-a-header",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(10).is_some(),
        "the app did not refuse:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    assert!(raw.contains("authorization"), "{raw}");
    assert!(
        !raw.contains("sk-from-a-header"),
        "the header value leaked into the error: {raw}"
    );
    assert!(server.requests().is_empty(), "no request may be sent");
    assert_eq!(pty.app_exit_code(), Some(1));
}

/// A `--header` with no `=` is a usage error whose message does not echo the argument: the
/// whole argument may be a pasted credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_malformed_header_error_does_not_echo_its_value() {
    let _serial = pty_guard();
    let dir = TempDir::new("header-bad");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "bad",
            "--header",
            "sk-secret-value",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(10).is_some(),
        "the app did not refuse:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    assert!(raw.contains("must be NAME=VALUE"), "{raw}");
    assert!(
        !raw.contains("sk-secret-value"),
        "the argument leaked into the error: {raw}"
    );
}

/// MCP servers are mounted from the same config file the CLI reads; a model call to one of
/// their tools goes through the approval prompt, returns its result, and the status bar shows
/// the servers and tools that connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_mcp_tools_run_through_approval_and_the_status_bar_shows_them() {
    let _serial = pty_guard();
    let dir = TempDir::new("mcp");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let log = dir.path().join("mcp.log");
    let config = mcp_config(dir.path(), &log);
    let server = MockServer::start(vec![
        tool_reply("call_1", "fx__echo", r#"{"hello": "mcp"}"#),
        text_reply("MCP-DONE"),
    ])
    .await;

    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mcp",
            "--workspace",
            ws.to_str().unwrap(),
            "--mcp-config",
            config.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // The session is open, so the status bar reports what connected. The fixture exposes 16
    // tools.
    assert!(
        pty.wait_for_screen("mcp 1/16", 20),
        "the status bar does not show the MCP counts:\n{}",
        pty.screen()
    );

    pty.send(b"call the echo tool\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval for the MCP tool:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("fx__echo"),
        "the prompt must name the tool:\n{}",
        pty.screen()
    );
    let before = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !before.contains("tools/call"),
        "the server ran the tool before approval was given: {before}"
    );

    pty.send(b"y");
    assert!(
        pty.wait_for_screen("MCP-DONE", 20),
        "the approved MCP call did not complete:\n{}",
        pty.screen()
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("tools/call"),
        "the fixture never received the call: {logged}"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0012 option B keeps MCP servers running outside the sandbox under `workspace-write`,
/// so the startup notice is the only thing that tells the user. A test holds it in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_the_workspace_write_notice_names_the_mcp_sandbox_gap() {
    let _serial = pty_guard();
    let dir = TempDir::new("notice");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "notice",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // The notice wraps across lines; collapse whitespace before matching the sentence.
    let notice = pty
        .screen()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notice.contains("MCP servers are NOT sandboxed"),
        "the startup notice must name the MCP sandbox gap: {notice}"
    );
    assert!(
        notice.contains("not a complete credential boundary"),
        "the notice must not claim credential isolation: {notice}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `--yes` removes the question, not the rules: a shell command under `workspace-write`
/// (which would ask) runs without a prompt, through the sandbox, and the decision is still
/// written to the audit record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_yes_runs_an_ask_without_a_prompt_and_still_audits_it() {
    let _serial = pty_guard();
    let dir = TempDir::new("yes");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "shell",
            // The output `sum=42` is not a substring of the command, so the assertion below
            // can only pass if the shell really ran.
            &json!({"command": "echo sum=$((6*7)); echo x | cat"}).to_string(),
        ),
        text_reply("YES-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "yes",
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    assert!(
        pty.screen()
            .contains("auto: tool calls are approved without asking"),
        "the startup must say approvals are off:\n{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("sum=42", 30),
        "the command output is missing, so the shell did not run:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("YES-DONE", 30),
        "the command did not run without a prompt:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "--yes must not show a prompt:\n{}",
        pty.screen()
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    let session = std::fs::read_to_string(sessions.join("yes.jsonl")).unwrap();
    assert!(
        session.contains("PermissionResolved") && session.contains("\"decision\":\"allow\""),
        "the decision must still be audited: {session}"
    );
}

/// `--max-model-calls` reaches the turn: with a limit of 1, the reply after the tool call is
/// never requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_max_model_calls_limits_the_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("max-calls");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "list", &json!({"path": "."}).to_string()),
        text_reply("SECOND-CALL"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "max-calls",
            "--workspace",
            ws.to_str().unwrap(),
            "--max-model-calls",
            "1",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    let path = sessions.join("max-calls.jsonl");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&path)
        .map(|text| text.contains("TurnFinished"))
        .unwrap_or(false)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish:\n{}",
            pty.screen()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests().len(), 1, "the limit of 1 was not applied");
    assert!(!pty.screen().contains("SECOND-CALL"), "{}", pty.screen());

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// An `@` reference attaches the file: the request the model receives carries a
/// `<file path="…">` block with the contents, and the transcript folds it to one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_an_at_reference_attaches_the_file_and_folds_it() {
    let _serial = pty_guard();
    let dir = TempDir::new("mention");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let server = MockServer::start(vec![text_reply("MENTION-OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mention",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send("看看 @notes.txt\r".as_bytes());
    assert!(
        pty.wait_for_screen("MENTION-OK", 20),
        "the reply is missing:\n{}",
        pty.screen()
    );
    assert_eq!(server.requests().len(), 1);
    let body = server.requests()[0].json();
    let user = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        user.contains("看看 @notes.txt"),
        "the text is unchanged: {user}"
    );
    // ADR 0018: the file is its own part, so the OpenAI adapter joins the parts
    // with a blank line and there is no `<file>` wrapper any more.
    assert!(
        user.contains("看看 @notes.txt\n\nalpha\nbeta"),
        "the attachment is a separate part: {user}"
    );
    assert!(!user.contains("<file path="), "{user}");
    // The transcript shows the folded form, not the file body.
    assert!(
        pty.wait_for_screen("[file: notes.txt,", 10),
        "the attachment is not folded:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("alpha"),
        "the file body must not be shown:\n{}",
        pty.screen()
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// A binary attachment is not sent at all: the request never reaches the model, a notice
/// names the file, and the input box still holds the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_binary_attachment_is_not_sent_and_the_input_is_kept() {
    let _serial = pty_guard();
    let dir = TempDir::new("mention-binary");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("blob.bin"), [0u8, 1, 2, 255]).unwrap();
    let server = MockServer::start(vec![text_reply("SHOULD-NOT-ARRIVE")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mention-binary",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"see @blob.bin\r");
    assert!(
        pty.wait_for_screen("not sent:", 10),
        "no failure notice:\n{}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "a binary attachment must not be sent"
    );
    assert!(pty.screen().contains("blob.bin"), "{}", pty.screen());
    assert!(
        pty.screen().contains("see @blob.bin"),
        "the input was lost:\n{}",
        pty.screen()
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `Tab` completes an `@` reference in the input box; `Enter` then attaches the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_tab_completes_an_at_reference_and_enter_attaches_it() {
    let _serial = pty_guard();
    let dir = TempDir::new("tab-mention");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let server = MockServer::start(vec![text_reply("TAB-OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "tab",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send("看 @no".as_bytes());
    pty.send(b"\t");
    assert!(
        pty.wait_for_screen("@notes.txt", 10),
        "Tab did not complete the reference:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("TAB-OK", 20),
        "the turn did not run:\n{}",
        pty.screen()
    );
    assert_eq!(server.requests().len(), 1);
    let body = server.requests()[0].json();
    let user = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(user.contains("看 @notes.txt"), "{user}");
    // The completion leaves a trailing space, so the attachment part starts after it; the
    // adapter joins the two parts with a blank line.
    assert!(user.contains("\n\nalpha\nbeta"), "{user}");
    assert!(!user.contains("<file path="), "{user}");

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The session directory, created empty by the caller, must stay empty when startup fails.
fn assert_session_dir_empty(dir: &Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.is_empty(),
        "a failed start left files behind: {names:?}"
    );
}

/// Acceptance 1: a `--workspace` that does not exist fails as a **workspace** error, names
/// the path, does not blame the sandbox, does not suggest `full-access`, and creates no
/// session file (dogfooding D1/D2).
#[test]
fn pty_a_missing_workspace_is_a_workspace_error_and_leaves_no_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("missing-workspace");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let missing = dir.path().join("no-such-workspace");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "missing-workspace",
            "--workspace",
            missing.to_str().unwrap(),
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit on a bad workspace"
    );
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw.contains(missing.to_str().unwrap()), "{raw}");
    assert!(raw.contains("workspace"), "{raw}");
    assert!(
        !raw.contains("sandbox backend"),
        "a bad workspace must not blame the sandbox: {raw}"
    );
    assert!(
        !raw.contains("--profile"),
        "a bad workspace must not suggest a profile (old or new name): {raw}"
    );
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "a refusal must not enter the alternate screen: {raw}"
    );
    assert_session_dir_empty(&session_dir);
}

/// ADR 0020: a `--session-dir` that is an ancestor of the workspace refuses at startup, says
/// why, does not blame the backend, and creates no session file.
#[test]
fn pty_a_session_dir_above_the_workspace_is_refused_and_leaves_no_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("nested-session");
    let session_dir = dir.path().join("sessions");
    let ws = session_dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "nested-session",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit on a nested workspace"
    );
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw.contains("workspace is inside"), "{raw}");
    assert!(
        !raw.contains("sandbox backend"),
        "a nested workspace must not blame the backend: {raw}"
    );
    assert!(!raw.contains("--profile"), "{raw}");
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "a refusal must not enter the alternate screen: {raw}"
    );
    // The workspace lives inside the session dir, so the dir is not empty; the point is that
    // no session file was created.
    assert!(
        !session_dir.join("nested-session.jsonl").exists(),
        "a failed start left a session file behind"
    );
}

/// Acceptance 2 and 4: a failed probe still refuses, leaves no session file, and the same
/// `--session-id` with corrected parameters then creates the session (dogfooding D2).
#[test]
fn pty_a_failed_probe_leaves_no_session_and_the_retry_creates_one() {
    let _serial = pty_guard();
    let dir = TempDir::new("probe-no-session");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "retry",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the refusal did not exit");
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains("sandbox backend is unavailable") || raw.contains("failed its probe"),
        "{raw}"
    );
    assert_session_dir_empty(&session_dir);

    // The corrected retry, same id: the session is created. `--panic-after-enter` exits
    // quickly and needs no server; the session is created before the terminal is entered.
    let mut retry = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "retry",
            "--workspace",
            ws.to_str().unwrap(),
            "--panic-after-enter",
        ],
        20,
        100,
    );
    assert!(retry.wait_for_exit(15).is_some(), "the retry did not exit");
    assert!(
        session_dir.join("retry.jsonl").exists(),
        "the corrected retry created the session"
    );
}

/// Acceptance 3: a missing or malformed MCP config file fails before the session exists.
#[test]
fn pty_a_bad_mcp_config_creates_no_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("bad-mcp");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, r#"{"servers": [], "bogus": 1}"#).unwrap();
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "bad-mcp",
            "--workspace",
            ws.to_str().unwrap(),
            // full-access keeps this test independent of the platform's sandbox backend; the
            // bad config is checked before any session file exists either way.
            "--profile",
            "full-access",
            "--mcp-config",
            bad.to_str().unwrap(),
        ],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the refusal did not exit");
    assert_eq!(pty.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw.contains(bad.to_str().unwrap()), "{raw}");
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "a bad MCP config must not enter the alternate screen: {raw}"
    );
    assert_session_dir_empty(&session_dir);
}

/// Acceptance 5: a clean exit prints one reason line on the normal screen, after the
/// alternate screen is gone, and the line ends with a newline (dogfooding D4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_clean_exit_prints_one_reason_line() {
    let _serial = pty_guard();
    let dir = TempDir::new("exit-reason");
    let server = MockServer::start(vec![text_reply("unused")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "exit-reason",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit");

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let after = raw
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    // The owner's UI review: the line carries a paste-ready command, not just the flag.
    let reason = "session exit-reason saved; resume: radian --resume exit-reason";
    let start = after
        .find(reason)
        .unwrap_or_else(|| panic!("no exit reason on the normal screen: {after}"));
    let rest = &after[start..];
    let end = rest
        .find('\n')
        .unwrap_or_else(|| panic!("the exit reason has no trailing newline: {rest}"));
    assert!(
        !rest[..end].contains("EXITED:"),
        "the reason line must end before the shell continues: {rest}"
    );
}

/// Acceptance 6: a panic writes `<id>.panic.log` with the time, thread and panic text, and
/// redacts header values and the key even when the panic text mentions them (dogfooding D4).
#[test]
fn pty_a_panic_writes_a_redacted_panic_log() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-log");
    let sessions = dir.path().join("sessions");
    let home = dir.path().to_str().unwrap();
    let secret = "SECRET-HEADER-VALUE-xyz";
    let key = "SECRET-KEY-abc";
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "panic-log",
            "--header",
            &format!("X-Test={secret}"),
            "--panic-message",
            &format!("boom {secret} and {key}"),
        ],
        &[("OPENAI_API_KEY", key), ("HOME", home)],
        20,
        100,
    );
    assert!(pty.wait_for_exit(15).is_some(), "the app did not exit");
    assert_eq!(pty.app_exit_code(), Some(101), "a panic exits 101");

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let after = raw
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    assert!(
        after.contains("panicked"),
        "the panic reason is on the normal screen: {after}"
    );

    // The log lives in the diagnostics directory (ADR 0013 decision 4), named for the session
    // and the time, not next to the session.
    let diagnostics = diagnostics_dir(dir.path());
    let log_path = std::fs::read_dir(&diagnostics)
        .unwrap_or_else(|e| panic!("no diagnostics directory {}: {e}", diagnostics.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.to_string_lossy().ends_with(".panic.log"))
        .unwrap_or_else(|| panic!("no panic log in {}", diagnostics.display()));
    let name = log_path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("panic-log-"),
        "the log names the session: {name}"
    );
    assert!(
        !sessions
            .read_dir()
            .map(|mut entries| entries.any(|entry| entry
                .map(|entry| entry.path().to_string_lossy().ends_with(".panic.log"))
                .unwrap_or(false)))
            .unwrap_or(false),
        "the panic log must not live in the session directory"
    );
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("no panic log at {}: {e}", log_path.display()));
    assert!(log.contains("boom"), "the panic text is in the log: {log}");
    assert!(log.contains("thread="), "the log names the thread: {log}");
    assert!(
        log.contains('<'),
        "the log has a timestamp token before the thread: {log}"
    );
    assert!(
        !log.contains(secret),
        "the header value must be redacted from the log: {log}"
    );
    assert!(
        !log.contains(key),
        "the key must be redacted from the log: {log}"
    );
    assert!(
        log.contains("<redacted>"),
        "redaction marker missing: {log}"
    );
    // Nothing on the terminal either: the hook replaces Rust's default one, which would print
    // the raw panic text to stderr.
    assert!(
        !raw.contains(secret) && !raw.contains(key),
        "the terminal output must not carry the header value or the key: {raw}"
    );
    assert!(
        raw.contains("<redacted>"),
        "the redacted report reaches stderr: {raw}"
    );
}

/// The pid of the app the pty's `sh -c` wrapper started.
fn app_pid(pty: &Pty) -> String {
    let shell = pty.child.process_id().expect("the wrapper shell has a pid");
    let out = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(shell.to_string())
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .expect("the app runs under the wrapper shell")
        .trim()
        .to_string()
}

/// Dogfooding D7: SIGTERM and SIGHUP leave like a quit. The terminal is restored, one reason
/// line names the signal and how to resume, and the exit code is 128 + the signal.
#[test]
fn pty_sigterm_and_sighup_restore_the_terminal_and_say_why() {
    for (signal, name, code) in [("TERM", "SIGTERM", 143), ("HUP", "SIGHUP", 129)] {
        let _serial = pty_guard();
        let dir = TempDir::new("signal");
        let id = format!("signal-{}", name.to_lowercase());
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                &id,
            ],
            20,
            100,
        );
        assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
        let pid = app_pid(&pty);
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(&pid)
            .status()
            .unwrap();
        assert!(status.success(), "kill -{signal} {pid}");
        assert!(
            pty.wait_for_exit(15).is_some(),
            "{name}: the app did not exit"
        );
        assert_eq!(pty.app_exit_code(), Some(code), "{name}: 128 + signal");
        let raw = String::from_utf8_lossy(&pty.raw()).to_string();
        let (_, after) = raw
            .rsplit_once("\u{1b}[?1049l")
            .unwrap_or_else(|| panic!("{name}: the alternate screen was not left: {raw}"));
        assert!(
            after.contains(&format!("received {name}"))
                && after.contains(&format!("--resume {id}")),
            "{name}: the reason line is on the normal screen: {after}"
        );
        assert!(
            raw.contains("icanon") && !raw.contains("-icanon"),
            "{name}: cooked mode restored"
        );
    }
}

// ---------- provider selection: anthropic (Phase 7 / anthost) ----------

/// One Anthropic text reply (the minimal end_turn stream).
fn anth_text_reply(text: &str) -> anth::Reply {
    anth::Reply::sse(&[
        anth::message_start(Some(json!({"input_tokens": 3, "output_tokens": 1}))),
        anth::text_start(0),
        anth::text_delta(0, text),
        anth::block_stop(0),
        anth::message_delta("end_turn", Some(json!({"output_tokens": 2}))),
        anth::message_stop(),
    ])
}

/// `--provider anthropic` in the TUI talks the Messages API and shows the reply; the session
/// snapshot names anthropic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_anthropic_provider_sends_a_messages_request_and_renders_the_reply() {
    let _serial = pty_guard();
    let server = anth::MockServer::start(vec![anth_text_reply("ANTHROPIC-TUI-REPLY")]).await;
    let dir = TempDir::new("anthropic");
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "claude",
            "--provider",
            "anthropic",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "anthro",
        ],
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-tui-key"),
            ("RADIAN_BASE_URL", &server.base_url),
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("ANTHROPIC-TUI-REPLY", 20),
        "the reply is not on screen:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some("sk-ant-tui-key"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));

    // The snapshot names the real provider, and never the key.
    let session = std::fs::read_to_string(dir.path().join("anthro.jsonl")).unwrap();
    assert!(session.contains("\"provider\":\"anthropic\""), "{session}");
    assert!(
        !session.contains("sk-ant-tui-key"),
        "the key reached the session file"
    );
    // ADR 0013: the key never reaches the terminal (startup line, errors, redraws).
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        !raw_text.contains("sk-ant-tui-key"),
        "the key reached the terminal"
    );
}

/// D15: naming an existing session with `--session-id` points at `--resume`, fails before the
/// terminal is entered, and does not touch the file.
#[test]
fn pty_session_id_already_exists_points_at_resume() {
    let _serial = pty_guard();
    let dir = TempDir::new("dup-id");
    // First run: create the session, then leave via the panic hook (quick, no server needed).
    let mut first = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "dup",
            "--panic-after-enter",
        ],
        20,
        80,
    );
    assert!(
        first.wait_for_exit(15).is_some(),
        "the first run did not exit"
    );
    assert!(dir.path().join("dup.jsonl").exists());
    let before = std::fs::read(dir.path().join("dup.jsonl")).unwrap();

    let mut second = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "dup",
        ],
        20,
        80,
    );
    assert!(
        second.wait_for_exit(15).is_some(),
        "the second run did not exit"
    );
    assert_eq!(second.app_exit_code(), Some(1));
    let raw = String::from_utf8_lossy(&second.raw()).to_string();
    assert!(raw.contains("already exists"), "{raw}");
    assert!(raw.contains("--resume dup"), "{raw}");
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "the refusal must not enter the alternate screen: {raw}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("dup.jsonl")).unwrap(),
        before,
        "the existing session must not be touched"
    );
}

/// `--thinking` and `--max-tokens` are Anthropic-only: under the default openai-compatible
/// provider the TUI refuses them before the terminal, and creates no session file.
#[test]
fn pty_anthropic_only_flags_are_refused_under_openai_compatible() {
    let _serial = pty_guard();
    // `--thinking` left this check in ADR 0026 D11 (it rides both wire protocols now); only
    // `--max-tokens` remains Anthropic-only.
    {
        let (label, extra) = ("max-tokens", ["--max-tokens", "100"]);
        let dir = TempDir::new(&format!("flag-{label}"));
        let session_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut args = vec![
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "flagged",
        ];
        args.extend(extra);
        let mut pty = Pty::spawn(&args, 20, 80);
        assert!(
            pty.wait_for_exit(15).is_some(),
            "{label}: the refusal did not exit"
        );
        assert_eq!(pty.app_exit_code(), Some(1), "{label}");
        let raw = String::from_utf8_lossy(&pty.raw()).to_string();
        assert!(raw.contains("anthropic"), "{label}: {raw}");
        assert!(
            !raw.contains("\u{1b}[?1049h"),
            "{label}: must fail before the terminal: {raw}"
        );
        assert!(
            std::fs::read_dir(&session_dir).unwrap().next().is_none(),
            "{label}: a session file was created"
        );
    }
}

/// ADR 0017: a model error ends the turn, not the session. The TUI shows the error as a
/// transcript notice, returns to the input box, and the next line runs a new turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_model_error_is_shown_and_the_session_continues() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        Reply::status(500, "provider is down"),
        text_reply("SECOND-OK"),
    ])
    .await;
    let dir = TempDir::new("model-error");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "recover",
        ],
        24,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("turn failed", 20),
        "the failure is not shown:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("HTTP 500", 20),
        "the HTTP status is not shown:\n{}",
        pty.screen()
    );
    // Still usable: a second line opens a new turn that succeeds.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("SECOND-OK", 20),
        "the session did not continue:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(server.requests().len(), 2, "both turns reached the model");
}

/// ADR 0018's fallback: a recognizable context-overflow error is shown, followed by the
/// one-line hint, and the session stays usable (the error ends the turn, ADR 0017).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_context_overflow_is_shown_with_a_hint() {
    let _serial = pty_guard();
    let body = r#"{"error":{"message":"This model's maximum context length is 16385 tokens. However, your messages resulted in 20000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let server = MockServer::start(vec![Reply::status(400, body), text_reply("SECOND-OK")]).await;
    let dir = TempDir::new("overflow");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "overflow",
        ],
        24,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("model context overflow", 20),
        "the overflow is not shown:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("compaction could not keep the request", 20),
        "the hint is not shown:\n{}",
        pty.screen()
    );
    // Still usable: a second line opens a new turn that succeeds.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("SECOND-OK", 20),
        "the session did not continue:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0018: the startup notice names the window and where it came from. An explicit
/// `--context-window` needs no catalog, so this is deterministic without a mock catalog.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_the_startup_notice_names_the_window_source() {
    let _serial = pty_guard();
    let dir = TempDir::new("catalog-window");
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "catalog-window",
            "--context-window",
            "20000",
            "--profile",
            "full",
        ],
        24,
        110,
    );
    assert!(
        pty.wait_for_screen("B 15000 from --context-window", 10),
        "the startup notice does not name the explicit window and its B:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// ADR 0021 D2/D6: the TUI shows the memory project state and the injection budget at
/// startup. The `HOME` is private to the pty, so this never reads the real `~/.radian`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_memory_notice_names_the_project_state_and_the_budget() {
    let _serial = pty_guard();
    let dir = TempDir::new("memory-notice");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mem-notice",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    let notice = pty
        .screen()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        notice.contains("[memory]"),
        "the memory startup notice is shown: {notice}"
    );
    assert!(
        notice.contains("no project linked"),
        "the notice names the unlinked state: {notice}"
    );
    assert!(
        notice.contains("injection on, up to 2000 tokens"),
        "the notice names the budget: {notice}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0021 D7: `/memory` commands run in the TUI without a model request, and the record
/// they create is injected into the next request's system prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_memory_commands_run_without_a_model_request() {
    let _serial = pty_guard();
    let dir = TempDir::new("memory-commands");
    let server = MockServer::start(vec![text_reply("REPLY-AFTER-MEMORY")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "memcmd",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());

    pty.send(b"/memory project new\r");
    assert!(pty.wait_for_screen("now owns", 10), "{}", pty.screen());
    pty.send(b"/memory add \"hello world\"\r");
    assert!(pty.wait_for_screen("added", 10), "{}", pty.screen());
    pty.send(b"/memory list\r");
    assert!(pty.wait_for_screen("hello world", 10), "{}", pty.screen());
    assert!(
        server.requests().is_empty(),
        "a /memory command must not reach the model"
    );

    // A normal message still works, and the memory is in its system prompt.
    pty.send(b"what do you remember?\r");
    assert!(
        pty.wait_for_screen("REPLY-AFTER-MEMORY", 20),
        "{}",
        pty.screen()
    );
    let body = server.requests()[0].json();
    let system = body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(system.contains("hello world"), "system prompt: {system}");

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// A `/memory` read command runs while a turn is stuck on a hanging model call and does not
/// interrupt it (ADR 0021 D6: memory takes effect at the next request, not mid-turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_memory_command_does_not_interrupt_a_running_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("memory-running");
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "memrun",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"/memory project new\r");
    assert!(pty.wait_for_screen("now owns", 10), "{}", pty.screen());
    pty.send(b"/memory add --global \"remembered\"\r");
    assert!(pty.wait_for_screen("added", 10), "{}", pty.screen());

    // Start a turn whose reply never completes; the status bar shows `running`.
    pty.send(b"start a long turn\r");
    assert!(pty.wait_for_screen("running", 10), "{}", pty.screen());
    // `running` is shown at `TurnStarted`, before the request reaches the server; wait for
    // the request itself (polling, like `wait_for_screen`) instead of asserting at once.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.requests().is_empty() && std::time::Instant::now() < deadline {
        pty.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests().len(), 1);

    pty.send(b"/memory list --global\r");
    assert!(pty.wait_for_screen("remembered", 10), "{}", pty.screen());
    assert!(
        pty.screen().contains("running"),
        "the turn is still running:\n{}",
        pty.screen()
    );
    assert_eq!(server.requests().len(), 1, "the command sent no request");

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0018: `/compact` runs without a model request and shows its result as a notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_compact_shows_its_result_without_a_model_request() {
    let _serial = pty_guard();
    let dir = TempDir::new("compact");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "compact",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"/compact\r");
    assert!(
        pty.wait_for_screen("nothing to compact", 15),
        "{}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "a /compact must not reach the model"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- the composer: editing, history, paste and drafts (ADR 0009 Phase 4) ----------

/// The last `user` message's text of the `n`-th recorded request.
fn last_user_text(server: &MockServer, n: usize) -> String {
    let body = server.requests()[n].json();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_editing_in_the_middle_sends_the_edited_text() {
    let _serial = pty_guard();
    let dir = TempDir::new("composer-edit");
    let server = MockServer::start(vec![text_reply("EDITED-OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "edit",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // "helo", move left twice (past `o` and `l`) and insert an `l`.
    pty.send(b"helo");
    pty.send(b"\x1b[D\x1b[D");
    pty.send(b"l");
    pty.send(b"\r");
    assert!(pty.wait_for_screen("EDITED-OK", 20), "{}", pty.screen());
    assert_eq!(last_user_text(&server, 0), "hello");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_multiline_history_and_paste_edit_at_the_cursor() {
    let _serial = pty_guard();
    let dir = TempDir::new("composer-multiline");
    let server = MockServer::start(vec![text_reply("MULTI-OK"), text_reply("PASTE-OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "multi",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // `one` Ctrl-J `two`, then Up to the end of the first line and append `!`.
    pty.send(b"one");
    pty.send(&[0x0a]); // Ctrl-J
    pty.send(b"two");
    pty.send(b"\x1b[A"); // Up: the end of `one`, keeping the column
    pty.send(b"!");
    pty.send(b"\r");
    assert!(pty.wait_for_screen("MULTI-OK", 20), "{}", pty.screen());
    assert_eq!(last_user_text(&server, 0), "one!\ntwo");

    // Paste inserts at the cursor, not at the end.
    pty.send(b"ac");
    pty.send(b"\x1b[D"); // Left
    pty.send(b"\x1b[200~b\x1b[201~");
    pty.send(b"\r");
    assert!(pty.wait_for_screen("PASTE-OK", 20), "{}", pty.screen());
    assert_eq!(last_user_text(&server, 1), "abc");

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_draft_is_written_on_exit_and_restored_by_resume() {
    let _serial = pty_guard();
    let dir = TempDir::new("composer-draft");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let draft = sessions.join("draft.draft");

    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "draft",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"unsent draft");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&draft).unwrap(),
        "unsent draft",
        "the draft is written on a clean exit"
    );

    // Resume the same session: the draft is restored into the input box and consumed.
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--resume",
            "draft",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("unsent draft", 10),
        "the draft is not in the input box:\n{}",
        pty.screen()
    );
    assert!(!draft.exists(), "a resume consumes the draft");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Waits until `path`'s contents contain `needle`. The file is the synchronization (the app
/// writes it from its own task); the deadline only bounds a failure.
fn wait_for_file(path: &Path, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if std::fs::read_to_string(path)
            .map(|text| text.contains(needle))
            .unwrap_or(false)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Writes a session with one user turn and one assistant reply, using the storage API. The
/// workspace must be the canonical string the index will match.
async fn seed_session(dir: &Path, id: &str, workspace: &str, user: &str) {
    let path = dir.join(format!("{id}.jsonl"));
    let journal = radian_session::SessionJournal::create(
        &path,
        id,
        radian_session::SessionCreatedData::new(workspace, json!({})),
    )
    .expect("create the session file");
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user(user),
        })
        .await
        .unwrap();
    journal
        .append(agent_core::JournalRecord::Message {
            index: 1,
            message: agent_core::Message::assistant("REPLY"),
        })
        .await
        .unwrap();
    drop(journal);
}

/// The picker lists this workspace's sessions, filters, resumes the chosen one, and the next
/// model request carries that session's history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_pick_lists_filters_and_resumes_the_chosen_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    seed_session(&sessions, "beta", &workspace, "BETA-HISTORY").await;
    // Another workspace's session must never be listed.
    seed_session(&sessions, "other", "/work/somewhere-else", "OTHER-HISTORY").await;

    let server = MockServer::start(vec![text_reply("PICKED-REPLY")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--pick",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("alpha", 10),
        "the picker did not list alpha:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("beta", 10),
        "the picker did not list beta:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("other"),
        "the picker listed another workspace's session:\n{}",
        pty.screen()
    );
    // Type a filter. Without filtering, Enter would resume the first row (alpha); resuming
    // beta proves the filter selected it.
    pty.send(b"beta");
    assert!(
        pty.wait_until(5, |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("resumed session beta", 10),
        "Enter did not resume the filtered row:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("BETA-HISTORY", 5),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    // The next request carries the resumed history, not the other session's.
    pty.send(b"continue\r");
    assert!(
        pty.wait_for_screen("PICKED-REPLY", 15),
        "the resumed session did not run a turn:\n{}",
        pty.screen()
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].body.contains("BETA-HISTORY"),
        "{}",
        requests[0].body
    );
    assert!(
        !requests[0].body.contains("ALPHA-HISTORY"),
        "{}",
        requests[0].body
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `n` starts a new session; `Esc` quits with code 0 and creates no session file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_pick_starts_new_and_escape_leaves_no_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick-new");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;

    let jsonl = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    };
    let before = jsonl(&sessions);

    // Esc: quit with code 0 and no new session file.
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--pick",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("alpha", 10), "{}", pty.screen());
    pty.send(&[0x1b]); // Esc
    assert!(pty.wait_for_exit(10).is_some(), "Esc did not quit");
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(jsonl(&sessions), before, "Esc created a session file");

    // `n`: a new session.
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--pick",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("alpha", 10), "{}", pty.screen());
    pty.send(b"n");
    assert!(
        pty.wait_for_screen("new session", 10),
        "`n` did not start a new session:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// `--pick` with no sessions goes straight to a new session with a notice (no empty list).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_pick_without_sessions_starts_a_new_one() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick-none");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--pick",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("no sessions for this workspace yet", 10),
        "the no-session notice is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("new session", 10),
        "the picker did not fall through to a new session:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// `/sessions` lists the current workspace's sessions with a copyable `--resume`; `/export`
/// writes the current session and refuses to overwrite without `--force`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_sessions_and_export_commands() {
    let _serial = pty_guard();
    let dir = TempDir::new("sess-cmd");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("SESSION-REPLY")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "cmd",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"session body text\r");
    assert!(pty.wait_for_screen("SESSION-REPLY", 15), "{}", pty.screen());

    // `/sessions` names the current session and the way to reopen it.
    pty.send(b"/sessions\r");
    assert!(
        pty.wait_for_screen("--resume cmd", 10),
        "the session list is missing the resume hint:\n{}",
        pty.screen()
    );

    // `/export` writes the default `<workspace>/radian-cmd.md`.
    pty.send(b"/export\r");
    assert!(
        pty.wait_for_screen("exported to", 10),
        "no export notice:\n{}",
        pty.screen()
    );
    let export = ws.join("radian-cmd.md");
    assert!(
        wait_for_file(&export, "session body text", 10),
        "the export file was not written with the session text"
    );

    // Without `--force` it refuses; with `--force` it overwrites.
    pty.send(b"/export\r");
    assert!(
        // The notice wraps, so match a fragment that stays on one line.
        pty.wait_for_screen("ready exists", 10),
        "the overwrite refusal is missing:\n{}",
        pty.screen()
    );
    std::fs::write(&export, "OLD").unwrap();
    pty.send(b"/export md radian-cmd.md --force\r");
    assert!(
        wait_for_file(&export, "session body text", 10),
        "the forced export did not overwrite the file"
    );
    let text = std::fs::read_to_string(&export).unwrap();
    assert!(!text.contains("OLD"), "{text}");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `/sessions` opens the picker overlay while a turn is running; Esc closes it and the turn
/// finishes normally (ADR 0023 U4). The reply is paced so the turn is still running when the
/// picker is opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_sessions_opens_and_closes_during_a_running_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("sessions-during-turn");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let Reply::Sse { pieces, .. } = Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "TURN-ONE-"}), None),
        chunk(json!({"role": "assistant", "content": "TURN-TWO"}), None),
        chunk(json!({}), Some("stop")),
    ]) else {
        unreachable!("Reply::sse builds an Sse reply")
    };
    let slow = Reply::Sse {
        pieces,
        pause: Duration::from_millis(1500),
    };
    let server = MockServer::start(vec![slow]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "during",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"start\r");
    // The first paced chunk has arrived, so the turn is running.
    assert!(
        pty.wait_for_screen("TURN-ONE-", 10),
        "the turn did not start:\n{}",
        pty.screen()
    );
    // Open the picker while the turn runs: it opens unfocused, so the composer keeps the keys.
    pty.send(b"/sessions\r");
    assert!(
        pty.wait_for_screen("filter:", 10),
        "the picker did not open during the turn:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]); // Esc closes the overlay
    assert!(
        pty.wait_until(5, |screen| !screen.contains("filter:")),
        "Esc did not close the picker:\n{}",
        pty.screen()
    );
    // The turn was not disturbed: its later chunk commits normally.
    assert!(
        pty.wait_for_screen("TURN-TWO", 15),
        "the turn did not finish after the picker closed:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- the configuration file (ADR 0008) ----------

/// A rebound `send` key sends, and `Enter` (rebound to newline in the same file) does not:
/// the request text carries the newline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_rebound_send_key_sends_and_enter_does_not() {
    let _serial = pty_guard();
    let dir = TempDir::new("config-send");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-s\"]\nnewline = [\"enter\"]\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("SENT")]).await;
    let mut pty = Pty::spawn(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "rebind",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hi");
    pty.send(b"\r"); // Enter: a newline, not a send
    pty.send(&[0x13]); // Ctrl-S: the rebound send
    assert!(pty.wait_for_screen("SENT", 15), "{}", pty.screen());
    assert_eq!(server.requests().len(), 1, "only Ctrl-S sent");
    assert_eq!(
        last_user_text(&server, 0),
        "hi\n",
        "Enter added the newline"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The `[config]` line is a startup diagnostic printed on the normal screen, **before** the
/// alternate screen is entered. Printed inside it, the raw text sits in cells that the
/// renderer's diff then skips (both buffers hold a space there), so it is never erased and
/// corrupts the first transcript row — a resumed history is exactly that row. Both the byte
/// order and the rendered row are pinned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_config_line_is_not_written_inside_the_alternate_screen() {
    let _serial = pty_guard();
    let dir = TempDir::new("config-screen");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "schema_version = 1\n").unwrap();
    let path = dir.path().join("screen-me.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "screen-me",
        radian_session::SessionCreatedData::new("/tmp", json!({})),
    )
    .expect("create the session file");
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("hello parent config"),
        })
        .await
        .unwrap();
    drop(journal);

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "screen-me",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("> hello parent config", 10),
        "the first transcript row is corrupted:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    let alt = raw
        .find("\u{1b}[?1049h")
        .expect("the alternate screen is entered");
    let line = raw.find("[config]").expect("the config line is printed");
    assert!(
        line < alt,
        "the config line is printed inside the alternate screen:\n{raw}"
    );
    assert!(
        !pty.screen().contains("[config]"),
        "the config line leaked into the alternate screen:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// One key bound to two actions refuses startup, naming the conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_keymap_conflict_refuses_to_start() {
    let _serial = pty_guard();
    let dir = TempDir::new("config-conflict");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-j\"]\n",
    )
    .unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "conflict",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_exit(10).is_some(),
        "the app must refuse to start"
    );
    assert_ne!(pty.app_exit_code(), Some(0));
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    assert!(raw.contains("ctrl-j") && raw.contains("newline"), "{raw}");
    assert!(!sessions.join("conflict.jsonl").exists(), "no session file");
}

/// `/fork` (ADR 0022): when idle it creates a child with the parent's history; while a turn
/// is running it is refused; the child resumes normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_fork_creates_a_resumable_child_when_idle() {
    let _serial = pty_guard();
    let dir = TempDir::new("fork-pty");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let server = MockServer::start(vec![text_reply("PARENT-DONE")]).await;
    let mut parent = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "parent",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(parent.wait_for_screen("message", 10), "{}", parent.screen());
    parent.send(b"hello parent\r");
    assert!(
        parent.wait_for_screen("PARENT-DONE", 15),
        "{}",
        parent.screen()
    );

    parent.send(b"/fork\r");
    assert!(
        parent.wait_for_screen("forked at seq", 10),
        "the fork notice is missing:\n{}",
        parent.screen()
    );
    // The child is the only new `.jsonl`.
    let child_id = std::fs::read_dir(&sessions)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.ends_with(".jsonl") && name != "parent.jsonl")
        .map(|name| name.trim_end_matches(".jsonl").to_owned())
        .expect("a child session file");
    assert!(
        parent.wait_for_screen(&format!("--resume {child_id}"), 10),
        "the notice does not name the child:\n{}",
        parent.screen()
    );
    parent.quit();
    assert_eq!(parent.app_exit_code(), Some(0));

    // The child resumes and its history is the parent's turn.
    let server = MockServer::start(vec![text_reply("CHILD-DONE")]).await;
    let mut child = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--resume",
            &child_id,
        ],
        30,
        110,
    );
    assert!(
        child.wait_for_screen("hello parent", 10),
        "the child history is not rendered:\n{}",
        child.screen()
    );
    child.send(b"continue\r");
    assert!(
        child.wait_for_screen("CHILD-DONE", 15),
        "{}",
        child.screen()
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].body.contains("hello parent"),
        "the request lacks the forked history: {}",
        requests[0].body
    );
    child.quit();
    assert_eq!(child.app_exit_code(), Some(0));
}

/// `/fork` while a turn is running is refused with a notice and creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_fork_refuses_while_a_turn_runs() {
    let _serial = pty_guard();
    let dir = TempDir::new("fork-pty-running");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // A shell tool call that sleeps keeps the turn running while the input box is free.
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "shell",
            &json!({"command": "sleep 5"}).to_string(),
        ),
        text_reply("NEVER"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "running",
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("sleep 5", 15),
        "the sleeping tool call did not start:\n{}",
        pty.screen()
    );

    pty.send(b"/fork\r");
    assert!(
        pty.wait_for_screen("finish the current turn before forking", 10),
        "a running turn must refuse /fork:\n{}",
        pty.screen()
    );
    // No child file was created.
    let jsonl: Vec<String> = std::fs::read_dir(&sessions)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    assert_eq!(jsonl, vec!["running.jsonl".to_owned()], "{jsonl:?}");
    pty.quit();
}

/// Phase 5: `Ctrl-F` searches the transcript and jumps to an earlier match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_find_jumps_to_an_earlier_match() {
    let _serial = pty_guard();
    let dir = TempDir::new("find-pty");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // A long transcript: the first reply holds the marker, seven more fill it.
    let mut replies = vec![text_reply("ALPHA-MARKER")];
    for i in 2..=8 {
        let filler = (0..12)
            .map(|j| format!("filler-{i}-line-{j}"))
            .collect::<Vec<_>>()
            .join("\n");
        replies.push(text_reply(&filler));
    }
    let server = MockServer::start(replies).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "find",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"m1\r");
    assert!(pty.wait_for_screen("ALPHA-MARKER", 15), "{}", pty.screen());
    for i in 2..=8 {
        pty.send(format!("m{i}\r").as_bytes());
        assert!(
            pty.wait_for_screen(&format!("filler-{i}-line-11"), 15),
            "{}",
            pty.screen()
        );
    }
    assert!(
        !pty.screen().contains("ALPHA-MARKER"),
        "the marker should have scrolled off:\n{}",
        pty.screen()
    );

    // Ctrl-F opens the box; typing jumps to the earlier match.
    pty.send(&[0x06]);
    pty.send(b"ALPHA");
    assert!(
        pty.wait_for_screen("match 1/1", 10),
        "the find status is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("ALPHA-MARKER", 10),
        "the match did not scroll into view:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- the terminal going away, and a panicking test's cleanup ----------

/// The terminal going away (the pty master closes; an ssh session drops) must end the app
/// cleanly, not leave it spinning. This drives the binary **directly** through a plain
/// `openpty` with **no controlling terminal**, so closing the master is a pure EOF (no
/// SIGHUP).
///
/// `--freeze-draw` makes the app stop drawing after the first frame: with no further writes,
/// closing the pty cannot be noticed by a failed draw, so this test exercises the **input**
/// `poll(2)` path and nothing else. (Without it the app still writes ~30 bytes per frame when
/// idle -- the cursor position -- so the draw path usually wins the race, and a mutation that
/// disables the `POLLHUP` check would slip through.)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_closed_master_ends_the_app_cleanly() {
    let _serial = pty_guard();
    let dir = TempDir::new("master-gone");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The sessions directory is a sibling of the workspace: the sandbox refuses a workspace
    // inside it (ADR 0020).
    let sessions = dir.path().join("sessions");
    let frozen = dir.path().join("frozen");
    let home = TempDir::new("master-gone-home");
    let (mut child, mut master) = spawn_direct(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "master-gone",
            "--offline-catalog",
            "--workspace",
            ws.to_str().unwrap(),
            "--freeze-draw",
            frozen.to_str().unwrap(),
        ],
        home.path(),
    );

    // The first frame is drawn (its marker exists); from now on the app writes nothing.
    let mut drained = Vec::new();
    let mut buf = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(20);
    while !frozen.exists() {
        // Drain the output: the first frame is larger than the pty buffer, so the app is
        // blocked in `draw` (and has not written the marker yet) until someone reads. The
        // master is non-blocking, so an empty read is a no-op, not a wait.
        if let Ok(n) = master.read(&mut buf) {
            drained.extend_from_slice(&buf[..n]);
        }
        assert!(
            Instant::now() < deadline,
            "the app never drew its first frame; path {frozen:?}; output: {}",
            String::from_utf8_lossy(&drained)
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the app exited before the frame"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Close the terminal: the master fd. No signal is sent (no controlling terminal).
    drop(master);

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the app did not exit within 5 s of its terminal closing: the input POLLHUP path \
             did not fire, and drawing is frozen (--freeze-draw) so the draw path cannot rescue \
             it either"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(0),
        "the terminal-gone exit is the ordinary clean one (exit 0, no signal): {status:?}"
    );
}

/// The remaining window: the main loop can be stuck **inside crossterm's reader** (the tty can
/// hang up between `poll(2)` and crossterm's read, or a half-read escape sequence can make it
/// read again), so the loop never sees the signal flag. `--block-input` parks the read after
/// writing a marker, standing in for that spin; a SIGHUP must still end the process, forced by
/// the signal task's watchdog with the shell convention 128+SIGHUP.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_hung_main_loop_still_exits_on_a_signal() {
    let _serial = pty_guard();
    let dir = TempDir::new("hung-loop");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.path().join("sessions");
    let marker = dir.path().join("blocked");
    let home = TempDir::new("hung-loop-home");
    let (mut child, _master) = spawn_direct(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "hung-loop",
            "--offline-catalog",
            "--workspace",
            ws.to_str().unwrap(),
            "--block-input",
            marker.to_str().unwrap(),
        ],
        home.path(),
    );

    // The loop is stuck once the marker exists (a state check, not a sleep for sync).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the app never entered the blocked input read"
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the app exited before the signal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The main loop cannot see this (it is parked), so the signal task's watchdog must exit.
    // SAFETY: signal delivery to our own child process.
    unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGHUP) };

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the hung main loop did not exit within 10 s of a signal"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(128 + 1),
        "a hung main loop must exit 128+SIGHUP: {status:?}"
    );
}

/// A pty test that panics must not leave the app behind: `Drop` runs during the unwind and
/// kills the whole process group. `portable-pty` calls `setsid()`, so the shell's pid is the
/// group id and the app it started is in the same group; an empty group means no orphan.
#[test]
fn a_panicking_pty_test_leaves_no_app_process() {
    let _serial = pty_guard();
    let dir = TempDir::new("panicking-test");
    let group_file = dir.path().join("group");
    let group_file_clone = group_file.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                "panicking-test",
            ],
            20,
            80,
        );
        assert!(pty.wait_for_screen("message", 20), "the app did not start");
        std::fs::write(
            &group_file_clone,
            pty.process_group().expect("the shell pid").to_string(),
        )
        .unwrap();
        panic!("this pty test panics on purpose");
    }));
    assert!(result.is_err(), "the closure must panic");

    let pgid: i32 = std::fs::read_to_string(&group_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while group_alive(pgid) {
        assert!(
            Instant::now() < deadline,
            "the app's process group {pgid} outlived the panicking test"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------- ADR 0023 U2: the event-driven loop ----------

/// The frame markers on the wire: `set_cursor_visible` writes one `?25h` per drawn frame (the
/// same count the Step 0 baseline used).
fn frame_count(pty: &Pty) -> usize {
    pty.raw()
        .windows(5)
        .filter(|window| *window == b"[?25h")
        .count()
}

/// ADR 0023 U2's zero-redraw rule: an idle screen is not redrawn at all. Before the
/// event-driven loop the app drew ~19 frames/s while nothing happened (`docs/perf-baseline.md`
/// §7); now the frame count over an idle window must not move.
#[test]
fn pty_an_idle_screen_is_not_redrawn() {
    let _serial = pty_guard();
    let dir = TempDir::new("idle-redraw");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "idle-redraw",
            "--offline-catalog",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", 10),
        "{}",
        pty.screen()
    );
    let before = frame_count(&pty);
    assert!(before >= 1, "the first frame is drawn");

    let window = std::time::Instant::now();
    while window.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let after = frame_count(&pty);
    pty.quit();
    assert_eq!(
        after, before,
        "an idle screen must not be redrawn (ADR 0023 U2)"
    );
}

/// ADR 0023 U2: a signal is an event, not a poll. The loop is blocked on its channel while the
/// app is idle; SIGTERM must take the ordinary exit path at once — far inside the signal task's
/// 3 s watchdog, which is what would fire if the loop still had to poll for the flag.
#[test]
fn pty_a_signal_while_idle_leaves_immediately() {
    let _serial = pty_guard();
    let dir = TempDir::new("signal-immediate");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "signal-immediate",
            "--offline-catalog",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", 10),
        "{}",
        pty.screen()
    );
    let pid = app_pid(&pty);
    let sent = std::time::Instant::now();
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(&pid)
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM {pid}");
    assert!(pty.wait_for_exit(5).is_some(), "the app did not exit");
    let elapsed = sent.elapsed();
    assert_eq!(pty.app_exit_code(), Some(143), "128 + SIGTERM");
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the signal must be an event, not the 3 s watchdog: it took {elapsed:?}"
    );
}

/// ADR 0023 U2's draw fairness: a producer that floods the channel must not starve the frame.
/// `--flood-facts` sends 20 000 facts as fast as it can (real producers are bounded by the turn
/// structure; this one is not, which is what makes the rule testable). The loop processes at
/// most `MAX_FACTS_PER_TURN` per turn, so frames keep appearing while the queue drains, and a
/// key is still handled.
#[test]
fn pty_a_flooding_producer_still_draws_and_answers_keys() {
    let _serial = pty_guard();
    let dir = TempDir::new("flood-facts");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "flood-facts",
            "--offline-catalog",
            "--flood-facts",
            "20000",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", 10),
        "{}",
        pty.screen()
    );
    let before = frame_count(&pty);

    // Sample while the flood drains: the frame count must keep moving (a loop that drained the
    // whole queue in one turn would draw once and then nothing for the rest of the window).
    let window = std::time::Instant::now();
    while window.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let drawn = frame_count(&pty) - before;
    assert!(
        drawn >= 100,
        "the flood starved the draw: {drawn} frames in 2 s"
    );

    // A key is still handled while the queue drains.
    pty.send(b"zzz");
    assert!(
        pty.wait_for_screen("zzz", 5),
        "the key was not handled: {}",
        pty.screen()
    );
    pty.quit();
}

// ---------- the mouse wheel (ADR 0023 U2, plan B) ----------

/// The SGR report for one wheel notch: `ESC[<64;x;yM` is up, `ESC[<65;x;yM` is down.
/// crossterm's `parse_csi` (`b'<'` at :171) dispatches these to `parse_csi_sgr_mouse`
/// (:718), and `parse_cb` (:776) maps button 4/5 to `ScrollUp`/`ScrollDown` (:789-790).
fn wheel(up: bool) -> Vec<u8> {
    if up {
        b"\x1b[<64;10;10M".to_vec()
    } else {
        b"\x1b[<65;10;10M".to_vec()
    }
}

/// The mouse-reporting sequences, as bytes in the raw output.
const MOUSE_ON: &str = "\u{1b}[?1000h";
const MOUSE_MOTION_ON: &str = "\u{1b}[?1002h";
const MOUSE_SGR_ON: &str = "\u{1b}[?1006h";
const MOUSE_OFF: &str = "\u{1b}[?1000l";
const MOUSE_MOTION_OFF: &str = "\u{1b}[?1002l";
const MOUSE_SGR_OFF: &str = "\u{1b}[?1006l";

/// The app enabled reporting and released it again. Button-motion (`?1002h`) is part of the
/// switch: the transcript drag-selection needs it (selection slice). The any-motion form
/// (`?1003h`) is still never sent — a pointer move must not flood the input thread
/// (ADR 0023 U2), which the no-frame-on-move test asserts behaviorally.
fn assert_mouse_released(pty: &Pty, label: &str) {
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains(MOUSE_ON),
        "{label}: reporting was never enabled: {raw}"
    );
    assert!(
        raw.contains(MOUSE_MOTION_ON),
        "{label}: button-motion was never enabled: {raw}"
    );
    assert!(
        raw.contains(MOUSE_SGR_OFF) && raw.contains(MOUSE_OFF) && raw.contains(MOUSE_MOTION_OFF),
        "{label}: the mouse was not released: {raw}"
    );
    assert!(
        !raw.contains("\u{1b}[?1003h"),
        "{label}: any-motion reporting must never be enabled: {raw}"
    );
}

/// dogfood p5: a wheel-up scrolls the transcript and leaves the input box alone. The old
/// alternate-scroll behavior turned the wheel into `Up`, which replaced the draft with the
/// previous history entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_wheel_scrolls_the_transcript_not_the_input_history() {
    let _serial = pty_guard();
    let dir = TempDir::new("mouse-scroll");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // A long transcript: the first reply scrolls off.
    let mut replies = vec![text_reply("FIRST-MARKER")];
    for i in 2..=8 {
        let filler = (0..12)
            .map(|j| format!("filler-{i}-line-{j}"))
            .collect::<Vec<_>>()
            .join("\n");
        replies.push(text_reply(&filler));
    }
    let server = MockServer::start(replies).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mouse",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"m1\r");
    assert!(pty.wait_for_screen("FIRST-MARKER", 15), "{}", pty.screen());
    for i in 2..=8 {
        pty.send(format!("m{i}\r").as_bytes());
        assert!(
            pty.wait_for_screen(&format!("filler-{i}-line-11"), 15),
            "{}",
            pty.screen()
        );
    }
    assert!(
        !pty.screen().contains("FIRST-MARKER"),
        "the marker should have scrolled off:\n{}",
        pty.screen()
    );
    // A draft in the input box; the last history entry is `m8`.
    pty.send(b"half-written-draft");
    assert!(
        pty.wait_for_screen("half-written-draft", 5),
        "{}",
        pty.screen()
    );
    assert!(String::from_utf8_lossy(&pty.raw()).contains(MOUSE_ON));

    // Enough notches (3 lines each) to bring the first reply back into view.
    for _ in 0..40 {
        pty.send(&wheel(true));
    }
    assert!(
        pty.wait_for_screen("FIRST-MARKER", 5),
        "the wheel did not scroll the transcript up:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("half-written-draft"),
        "the wheel replaced the input draft with history:\n{}",
        pty.screen()
    );

    // Wheel down returns toward the bottom.
    for _ in 0..40 {
        pty.send(&wheel(false));
    }
    assert!(
        pty.wait_until(5, |screen| !screen.contains("FIRST-MARKER")),
        "the wheel did not scroll the transcript back down:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_mouse_released(&pty, "normal quit");
}

/// A `Moved` report is not input: it is dropped before the channel (so it cannot cause a
/// frame). The app also never enables any-motion reporting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_mouse_move_is_dropped() {
    let _serial = pty_guard();
    let dir = TempDir::new("mouse-move");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![text_reply("R")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "move",
        ],
        20,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"draft");
    assert!(pty.wait_for_screen("draft", 5), "{}", pty.screen());
    // A motion report (`ESC[<35;x;yM` = Moved) and a click must change nothing: the draft
    // stays, and the app never enabled any-motion reporting.
    pty.send(b"\x1b[<35;10;10M");
    pty.send(b"\x1b[<0;10;10M");
    // The next key proves the app is alive and the draft survived.
    pty.send(b"-ok");
    assert!(
        pty.wait_for_screen("draft-ok", 5),
        "a mouse move or click consumed input:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        !raw.contains("\u{1b}[?1003h"),
        "any-motion reporting must never be enabled: {raw}"
    );
    pty.quit();
}

/// `tui.mouse = false` sends no mouse sequence at all, so the terminal keeps its native
/// selection/copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_mouse_off_sends_no_reporting_sequence() {
    let _serial = pty_guard();
    let dir = TempDir::new("mouse-off");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "schema_version = 1\n[tui]\nmouse = false\n").unwrap();
    let server = MockServer::start(vec![text_reply("R")]).await;
    let mut pty = Pty::spawn(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mouse-off",
        ],
        20,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        !raw.contains(MOUSE_ON),
        "mouse off must not enable reporting: {raw}"
    );
    assert!(
        !raw.contains(MOUSE_OFF) && !raw.contains(MOUSE_SGR_ON) && !raw.contains(MOUSE_SGR_OFF),
        "mouse off must send no mouse sequence: {raw}"
    );
}

/// Every exit path releases the mouse: the ordinary quit, a panic, and a signal.
#[test]
fn pty_every_exit_path_releases_the_mouse() {
    // The ordinary quit (the `Drop` of the guard).
    {
        let _serial = pty_guard();
        let dir = TempDir::new("mouse-quit");
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                "quit",
            ],
            20,
            100,
        );
        assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
        pty.quit();
        assert_eq!(pty.app_exit_code(), Some(0));
        assert_mouse_released(&pty, "quit");
    }
    // The panic hook restores the terminal (and releases the mouse).
    {
        let _serial = pty_guard();
        let dir = TempDir::new("mouse-panic");
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                "panic",
                "--panic-after-enter",
            ],
            20,
            100,
        );
        assert!(
            pty.wait_for_exit(10).is_some(),
            "the app did not exit on panic"
        );
        assert_mouse_released(&pty, "panic");
    }
    // A signal leaves through the ordinary clean-exit path.
    {
        let _serial = pty_guard();
        let dir = TempDir::new("mouse-signal");
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--session-id",
                "signal",
            ],
            20,
            100,
        );
        assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
        let pid = app_pid(&pty);
        let status = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(&pid)
            .status()
            .unwrap();
        assert!(status.success(), "kill -TERM {pid}");
        assert!(pty.wait_for_exit(15).is_some(), "the app did not exit");
        assert_eq!(pty.app_exit_code(), Some(143));
        assert_mouse_released(&pty, "signal");
    }
}

/// With an overlay open, the wheel scrolls the overlay, not the transcript: the picker's
/// selected row moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_wheel_scrolls_an_open_overlay() {
    let _serial = pty_guard();
    let dir = TempDir::new("mouse-overlay");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA").await;
    seed_session(&sessions, "beta", &workspace, "BETA").await;
    let server = MockServer::start(vec![text_reply("R")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "current",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        24,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"/sessions\r");
    assert!(
        pty.wait_for_screen("filter:", 10),
        "the picker did not open:\n{}",
        pty.screen()
    );
    // The header's `--resume <id>` hint is drawn with the picker's rows; `filter:` alone can
    // be up a frame earlier under load. Wait for both before snapshotting.
    assert!(
        pty.wait_until(10, |screen| screen.contains("filter:")
            && screen.contains("--resume ")),
        "the picker's rows did not appear:\n{}",
        pty.screen()
    );
    let before = pty.screen();
    let before_hint = before
        .split("--resume ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a --resume hint")
        .to_string();
    // One notch down moves the picker's selection; the hint names a different session.
    pty.send(&wheel(false));
    assert!(
        pty.wait_until(5, |screen| {
            screen
                .split("--resume ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                != Some(before_hint.as_str())
        }),
        "the wheel did not move the picker's selection:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- the `--pick` overlay (ADR 0023 U4, plan B) ----------

/// `--pick` is the in-process picker overlay: the process enters the alternate screen **once**,
/// the chosen session's id expands `{session_id}` in a header, and the same terminal then runs
/// the session. This pins both the single terminal setup and the header ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_pick_resumes_the_selected_id_and_enters_once() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick-overlay");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    seed_session(&sessions, "beta", &workspace, "BETA-HISTORY").await;

    let server = MockServer::start(vec![text_reply("PICKED")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--pick",
            "--header",
            "x-s={session_id}",
        ],
        24,
        100,
    );
    // The picker overlay is up (still in the first, pre-session phase of the one loop).
    assert!(pty.wait_for_screen("filter:", 10), "{}", pty.screen());
    // Filter to beta and resume it.
    pty.send(b"beta");
    assert!(
        pty.wait_until(5, |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("resumed session beta", 10),
        "the picker did not resume the selection:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("BETA-HISTORY", 5),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(pty.wait_for_screen("PICKED", 15), "{}", pty.screen());
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].header("x-s"),
        Some("beta"),
        "the header must expand to the *selected* id, not a generated one"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    // The alternate screen is entered exactly once for the whole process: the picker and the
    // session share it.
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert_eq!(
        raw.matches("\u{1b}[?1049h").count(),
        1,
        "the alternate screen must be entered once:\n{raw}"
    );
}

/// The signal watchdog's forced exit restores the terminal too: a still-alive terminal must
/// stop receiving mouse reports after the app is gone. `--slow-draw` makes the main loop miss
/// the 3 s watchdog (the reader runs on its own thread since ADR 0023 U2, so `--block-input`
/// no longer parks the loop).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_signal_watchdog_releases_the_mouse() {
    let _serial = pty_guard();
    let dir = TempDir::new("mouse-watchdog");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mouse-watchdog",
            "--workspace",
            ws.to_str().unwrap(),
            "--slow-draw",
            "10000",
        ],
        20,
        100,
    );
    // The app wrote its mouse-on sequences before the (slow) first frame, so the loop is now
    // sleeping in `draw_frame` and cannot reach `confirm_exit`.
    assert!(
        pty.wait_for_raw(MOUSE_ON, 10),
        "the app never enabled mouse reporting"
    );
    let pid = app_pid(&pty);
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(&pid)
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM {pid}");
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the watchdog did not force the exit"
    );
    assert_eq!(pty.app_exit_code(), Some(143), "128 + SIGTERM");
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains(MOUSE_OFF) && raw.contains(MOUSE_SGR_OFF),
        "the watchdog exit must release the mouse: {raw}"
    );
}

/// A signal during the pre-session `--pick` phase leaves exactly like the session phase: exit
/// `128 + signal`, the reason line, and the terminal (mouse included) restored. Nothing is
/// created, because the session id is not even resolved yet.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_signal_during_pick_exits_128_plus_signal() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick-signal");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    let jsonl = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    };
    let before = jsonl(&sessions);

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--pick",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("filter:", 10),
        "the picker did not open:\n{}",
        pty.screen()
    );
    let pid = app_pid(&pty);
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(&pid)
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM {pid}");
    assert!(
        pty.wait_for_exit(15).is_some(),
        "the app did not exit after the signal"
    );
    assert_eq!(pty.app_exit_code(), Some(143), "128 + SIGTERM");
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let (_, after) = raw
        .rsplit_once("\u{1b}[?1049l")
        .unwrap_or_else(|| panic!("the alternate screen was not left: {raw}"));
    assert!(
        after.contains("received SIGTERM"),
        "the reason line is missing: {after}"
    );
    assert!(
        raw.contains(MOUSE_OFF) && raw.contains(MOUSE_SGR_OFF),
        "the mouse was not released: {raw}"
    );
    assert_eq!(
        jsonl(&sessions),
        before,
        "a signal during picking must create no session file"
    );
}

/// The watchdog's forced exit must put back the stdout `O_NONBLOCK` it set: that flag belongs to
/// the **open file description**, shared with the shell and later programs on the same terminal
/// (a `dup` shares it), so leaving it set would make their writes fail with EAGAIN. The test
/// keeps a `dup` of the child's stdout (the pty slave) and checks `F_GETFL` on that same
/// description after the watchdog exits.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_watchdog_restores_the_stdout_flags() {
    use std::os::fd::AsRawFd;
    let _serial = pty_guard();
    let dir = TempDir::new("mouse-flags");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let home = TempDir::new("mouse-flags-home");
    let (mut child, mut master, slave) = spawn_direct_keeping_slave(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "flags",
            "--offline-catalog",
            "--workspace",
            ws.to_str().unwrap(),
            "--slow-draw",
            "10000",
        ],
        home.path(),
        None,
    );
    // Wait for the mouse-on sequence: the app has entered the terminal and is now sleeping in
    // `draw_frame`, so the signal below reaches the watchdog.
    let mut drained = Vec::new();
    let mut buf = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(20);
    while !String::from_utf8_lossy(&drained).contains("\u{1b}[?1000h") {
        if let Ok(n) = master.read(&mut buf) {
            drained.extend_from_slice(&buf[..n]);
        }
        assert!(
            Instant::now() < deadline,
            "the app never enabled mouse reporting: {}",
            String::from_utf8_lossy(&drained)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: signal delivery to our own child process.
    unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the watchdog did not force the exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // `slave` is a `dup` of the child's stdout: the same open file description, so the flags the
    // watchdog set (and must have restored) are visible here.
    let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL on the kept slave failed");
    assert_eq!(
        flags & libc::O_NONBLOCK,
        0,
        "the watchdog left O_NONBLOCK set on the shared terminal description"
    );
}

// ---------- ADR 0024: the structured question (ask_user) ----------

/// The `ask_user` arguments the ask tests use: one single, one multi, one text; all required.
fn ask_questions() -> serde_json::Value {
    json!({"questions": [
        {"id": "loader", "prompt": "How should it load?", "kind": "single",
         "options": [{"id": "builtin", "label": "Built-in"}, {"id": "process", "label": "Plugins"}],
         "required": true},
        {"id": "tiers", "prompt": "Which tiers ship?", "kind": "multi",
         "options": [{"id": "safe", "label": "Safe"}, {"id": "workspace", "label": "Workspace"}],
         "required": true},
        {"id": "name", "prompt": "What is it called?", "kind": "text", "required": true}
    ]})
}

/// `ask_user` with those questions, then the text reply.
fn ask_reply(done: &str) -> Vec<Reply> {
    vec![
        tool_reply("call_ask", "ask_user", &ask_questions().to_string()),
        text_reply(done),
    ]
}

/// ADR 0024 Q5/Q7 end to end: the TUI registers `ask_user`, shows the form, and the turn
/// continues after three answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_ask_user_answers_three_questions_and_the_turn_continues() {
    let _serial = pty_guard();
    // The workspace is a **sibling** of the session directory: the sandbox denies read on the
    // sessions directory, so a workspace inside it could not be both hidden and usable (the
    // linux3 finding).
    let dir = TempDir::new("ask");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(ask_reply("ASK-DONE")).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "ask",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        40,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    // ADR 0024 Q5: this host has an interactive user, so the startup line says so.
    assert!(
        pty.wait_for_screen("ask_user: on", 10),
        "the startup line must say ask_user is on: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("How should it load?", 20),
        "no question form: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("question 1/3", 20),
        "the title must name the position: {}",
        pty.screen()
    );
    // Question 1 (single): select the highlighted option.
    pty.send(b" ");
    pty.send(b"\t");
    // Question 2 (multi): select the highlighted option.
    pty.send(b" ");
    pty.send(b"\t");
    // Question 3 (text): type.
    pty.send(b"radian");
    // Enter -> the overview, Enter -> submit.
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("overview", 20),
        "the overview must be shown: {}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("ASK-DONE", 30),
        "the turn must continue after the answers: {}",
        pty.screen()
    );
}

/// ADR 0024 Q4: Ctrl-C while a question is up cancels the turn, the form goes with the waiter
/// (ADR 0023 U7), and **no** answer is sent: the model is never asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_ctrl_c_during_a_question_cancels_without_an_answer() {
    let _serial = pty_guard();
    let dir = TempDir::new("ask-cancel");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(json!({"role": "assistant", "content": "WORKING"}), None),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "call_ask", "type": "function",
                    "function": {"name": "ask_user", "arguments": ask_questions().to_string()}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        text_reply("ASK-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "ask-cancel",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        40,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("How should it load?", 20),
        "no question form: {}",
        pty.screen()
    );
    // The streamed text before the question is drawn normally, not lost.
    assert!(
        pty.screen().contains("WORKING"),
        "the streamed text must still be on screen: {}",
        pty.screen()
    );
    pty.send(&[0x03]);
    // The form's own title, not the prompt: the transcript shows the tool call's arguments,
    // which contain the prompt text.
    assert!(
        pty.wait_until_gone("question 1/3", 20),
        "the form must go with the cancelled wait: {}",
        pty.screen()
    );
    // One request only: the tool never returned an answer, so the model was never asked again.
    assert_eq!(
        server.requests().len(),
        1,
        "a cancelled question must not produce an answer"
    );
    assert!(!pty.screen().contains("ASK-DONE"));
}

/// ADR 0023 U7 end to end: two tool calls that need approval are asked one after the other, in
/// order, and the status bar counts the queue. The second never replaces the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_two_approvals_are_shown_in_order() {
    let _serial = pty_guard();
    let dir = TempDir::new("two-approvals");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("c1", "write", r#"{"path":"a.txt","content":"one"}"#),
        tool_reply("c2", "write", r#"{"path":"b.txt","content":"two"}"#),
        text_reply("BOTH-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "two-approvals",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval prompt: {}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("a.txt"),
        "the first prompt names the first path: {}",
        pty.screen()
    );
    // ADR 0023 U7: the status bar counts the pending queue.
    assert!(
        pty.wait_for_screen("1 pending", 10),
        "the status bar must count the queue: {}",
        pty.screen()
    );
    pty.send(b"y");
    // The transcript shows the second tool call's arguments early, and the box's resolved
    // `path:` line wraps at this width (the temp directory's name grows with the test index), so
    // neither `b.txt` nor `path: …` identifies the prompt. A box-only marker is needed: the box
    // no longer repeats the path in `args` (approval-fit dedups it), so the *pretty-printed*
    // `content` field (with the space) marks the box, while the transcript's tool-call line has
    // the compact form.
    let second = "\"content\": \"two\"";
    let first = "\"content\": \"one\"";
    assert!(
        pty.wait_until(20, |screen| screen.contains("approve tool call?")
            && screen.contains(second)),
        "the second approval must follow the first: {}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains(first),
        "the second prompt is shown alone, not merged with the first: {}",
        pty.screen()
    );
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("BOTH-DONE", 20),
        "the turn must continue after both approvals: {}",
        pty.screen()
    );
}

/// A long command's approval still shows `[y] allow once` (the keys are the box's own last
/// row), and answering `y` runs the whole command (approval-fit, owner acceptance).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_long_command_keeps_the_approval_keys_visible() {
    let _serial = pty_guard();
    let dir = TempDir::new("approval-fit");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // Sixty filler lines put the body far past what any box can show; the last line is the one
    // that must run.
    let mut command = String::new();
    for index in 0..60 {
        command.push_str(&format!("true # filler-{index}\n"));
    }
    command.push_str("echo APPROVAL-FIT-RAN");
    let arguments = json!({"command": command}).to_string();
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &arguments),
        text_reply("DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "approval-fit",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval prompt: {}",
        pty.screen()
    );
    // The box is one widget, but the first frame after the prompt can still be a partial
    // paint under load (observed: title and first rows drawn, key line not yet). Wait for a
    // frame that has the whole key line and the window indicator, not for one snapshot.
    assert!(
        pty.wait_until(10, |screen| screen.contains("[y] allow once")
            && screen.contains("[n] deny")
            && screen.contains("lines 1–")),
        "the key line is not fully on screen: {}",
        pty.screen()
    );
    // Scrolling the body keeps the keys on screen.
    pty.send(b"\x1b[B"); // Down
    assert!(
        pty.wait_until(10, |screen| screen.contains("lines 2–")
            && screen.contains("[y] allow once")),
        "the box must scroll with the keys visible: {}",
        pty.screen()
    );
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("APPROVAL-FIT-RAN", 20),
        "y must run the command: {}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// `/model` switches the wire: after the switch the requests go to the **new** mock server
/// with the new protocol (ADR 0026 D5, D6 — here opencode-go → anthropic).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_model_switch_sends_the_next_turn_to_the_new_provider() {
    let _serial = pty_guard();
    let dir = TempDir::new("model-switch");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let openai_server = MockServer::start(vec![text_reply("FIRST-PROVIDER-OK")]).await;
    let anthropic_server =
        anth::MockServer::start(vec![anth_text_reply("SWITCHED-PROVIDER-OK")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "m",
            "--base-url",
            &openai_server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "model-switch",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-switch-test"),
            ("RADIAN_BASE_URL", &anthropic_server.base_url),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"one\r");
    assert!(
        pty.wait_for_screen("FIRST-PROVIDER-OK", 20),
        "the first turn goes to the startup provider: {}",
        pty.screen()
    );
    // The switch: the anthropic descriptor's endpoint comes from the credential's env. The
    // first Enter accepts the slash completion (the owner's rule: Enter picks, never runs);
    // the second submits the line.
    pty.send(b"/model anthropic/claude-x\r\r");
    assert!(
        pty.wait_for_screen("next turn: anthropic/claude-x", 10),
        "the status bar shows the queued switch: {}",
        pty.screen()
    );
    pty.send(b"two\r");
    assert!(
        pty.wait_for_screen("SWITCHED-PROVIDER-OK", 20),
        "the next turn goes to the new provider: {}",
        pty.screen()
    );
    // The Messages API request reached the anthropic mock, with the key from auth.json.
    let requests = anthropic_server.requests();
    assert_eq!(requests.len(), 1, "one anthropic request");
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some("sk-ant-switch-test"));
    // The status marker is spent (the transcript's notice stays, by design).
    assert!(
        pty.wait_until(10, |screen| !screen.contains("next: anthropic/claude-x")),
        "the switch applied: {}",
        pty.screen()
    );
    // D5: the durable change is a route; the auth.json key is nowhere in the journal.
    let journal = std::fs::read_to_string(sessions.join("model-switch.jsonl")).unwrap();
    assert!(
        !journal.contains("sk-ant-switch-test"),
        "the key reached the journal"
    );
    assert!(
        journal.contains("\"model{\"provider\":\"anthropic\"") || journal.contains("anthropic"),
        "the switch is journaled"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// D8: a resume uses the last switched model from the journal, with no flags.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_resume_replays_the_switched_model() {
    let _serial = pty_guard();
    let dir = TempDir::new("model-resume");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let openai_server = MockServer::start(vec![text_reply("OPENAI-RESUME")]).await;
    let anthropic_server = anth::MockServer::start(vec![anth_text_reply("ANTHROPIC-RESUME")]).await;
    // Turn 1 on the startup provider, then the switch, then turn 2 on anthropic.
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "m",
            "--base-url",
            &openai_server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "model-resume",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-resume-test"),
            ("RADIAN_BASE_URL", &anthropic_server.base_url),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"go\r");
    assert!(pty.wait_for_screen("OPENAI-RESUME", 20), "{}", pty.screen());
    pty.send(b"/model anthropic/claude-x\r\r");
    assert!(
        pty.wait_for_screen("next turn: anthropic/claude-x", 10),
        "{}",
        pty.screen()
    );
    pty.send(b"again\r");
    assert!(
        pty.wait_for_screen("ANTHROPIC-RESUME", 20),
        "{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
    drop(pty);

    // Resume with no model flags: the journal's last change decides.
    let anthropic_server2 =
        anth::MockServer::start(vec![anth_text_reply("RESUMED-ON-ANTHROPIC")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--resume",
            "model-resume",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-resume-test"),
            ("RADIAN_BASE_URL", &anthropic_server2.base_url),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"third\r");
    assert!(
        pty.wait_for_screen("RESUMED-ON-ANTHROPIC", 20),
        "the resumed session stays on the switched model: {}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// `/thinking medium` on deepseek-v4.1-flash clamps **up** to high (ADR 0026 D11: it supports
/// low/high/max), names the clamp, and the next request carries the wire value as
/// `reasoning_effort`. `medium` matches no candidate, so Enter submits the typed level.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_thinking_clamps_up_and_sends_reasoning_effort() {
    let _serial = pty_guard();
    let dir = TempDir::new("thinking-clamp");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("THINKING-OK")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--provider",
            "opencode-go",
            "--model",
            "deepseek-v4.1-flash",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "thinking-clamp",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[
            ("OPENCODE_GO_API_KEY", "sk-go-thinking"),
            ("RADIAN_BASE_URL", &server.base_url),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"/thinking medium\r");
    assert!(
        pty.wait_for_screen("medium → high", 10),
        "the clamp is named: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("next turn: thinking high", 10),
        "the clamped level is queued: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(pty.wait_for_screen("THINKING-OK", 20), "{}", pty.screen());
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request");
    assert_eq!(
        requests[0].json()["reasoning_effort"],
        "high",
        "the clamped level is on the wire: {}",
        requests[0].json()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// Review M1: a valueless `--resume` decides the session in the picker, and the model block
/// runs **after** it, so the journal's last model change is the one resolved (status bar and
/// request alike). The config's model (an OpenAI mock) must not be used.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_picked_resume_uses_the_journals_model() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick-model");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // A session whose journal switched to anthropic/claude-x.
    let path = sessions.join("switched.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "switched",
        radian_session::SessionCreatedData::new(&workspace, json!({})),
    )
    .unwrap();
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("earlier"),
        })
        .await
        .unwrap();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::ConfigChanged {
            revision: 1,
            change: radian_host::ModelRoute::new("anthropic", "claude-x").to_value(),
        })
        .await
        .unwrap();
    drop(journal);

    // The config's model is the OpenAI mock; the journal says anthropic.
    let openai_server = MockServer::start(vec![text_reply("CONFIG-MODEL")]).await;
    let anthropic_server = anth::MockServer::start(vec![anth_text_reply("JOURNAL-MODEL")]).await;
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"mock\"\n\
             base_url = \"{}\"\n",
            openai_server.base_url
        ),
    )
    .unwrap();
    let mut pty = Pty::spawn_with_env(
        &[
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--resume",
        ],
        &[
            ("RADIAN_CONFIG", config.to_str().unwrap()),
            ("ANTHROPIC_API_KEY", "sk-ant-picked"),
            ("RADIAN_BASE_URL", &anthropic_server.base_url),
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("switched", 10),
        "the picker did not list the session: {}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("message", 10),
        "the picked session did not open: {}",
        pty.screen()
    );
    // The status bar names the journal's model, not the config's.
    assert!(
        pty.wait_until(10, |screen| screen.contains("claude-x")),
        "the status bar must show the journal's model: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("JOURNAL-MODEL", 20),
        "the request must go to the journal's provider: {}",
        pty.screen()
    );
    assert!(
        openai_server.requests().is_empty(),
        "the config model must not be used"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// ADR 0024 Q5: `--approval never` only affects approvals. The user is still there, so the
/// TUI still registers `ask_user`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_approval_never_still_registers_ask_user() {
    let _serial = pty_guard();
    let dir = TempDir::new("ask-never");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("NEVER-DONE")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "ask-never",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "never",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    assert!(
        pty.wait_for_screen("ask_user: on", 10),
        "`--approval never` must not turn ask_user off: {}",
        pty.screen()
    );
}

/// ADR 0024 Q5: `radian-tui` registers `ask_user` even with **no** `--workspace` (so no coding
/// tools). The startup line must reflect the actual registration, not the host's intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_ask_user_is_registered_without_a_workspace() {
    let _serial = pty_guard();
    let dir = TempDir::new("ask-no-ws");
    let server = MockServer::start(vec![text_reply("NO-WS-DONE")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "ask-no-ws",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    assert!(
        pty.wait_for_screen("ask_user: on", 10),
        "the startup line must say ask_user is on: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("NO-WS-DONE", 20),
        "the turn must run: {}",
        pty.screen()
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{}", pty.screen());
    let body = &requests[0].body;
    assert!(
        body.contains("ask_user"),
        "the request's tool list must offer ask_user without --workspace: {body}"
    );
    // The owner's UI review changed this: the current directory is the coding root now, so a
    // session without `--workspace` does have the file tools.
    assert!(
        body.contains("\"name\":\"read\""),
        "the current directory is the coding root: {body}"
    );
}

/// The slash command tree completes every level: typing `/mem` lists the
/// commands, Tab completes the name, the next level's list appears, and Tab completes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_slash_completion_walks_the_command_tree() {
    let _serial = pty_guard();
    let dir = TempDir::new("slash-completion");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "slash",
            "--offline-catalog",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", 10),
        "{}",
        pty.screen()
    );

    pty.send(b"/mem");
    assert!(
        pty.wait_for_screen("/memory", 10),
        "the command list did not appear:\n{}",
        pty.screen()
    );
    pty.send(b"\t");
    assert!(
        pty.wait_for_screen("> /memory ", 10),
        "Tab did not complete the command name:\n{}",
        pty.screen()
    );
    // The next level's list is already up; a partial is fuzzy-matched and completed.
    pty.send(b"fo");
    assert!(
        pty.wait_for_screen("forget", 10),
        "the subcommand list did not appear:\n{}",
        pty.screen()
    );
    pty.send(b"\t");
    assert!(
        pty.wait_for_screen("> /memory forget", 10),
        "Tab did not complete the subcommand:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- the default workspace ----------

/// Without `--workspace` the current directory is the coding root, so the first request carries
/// the file tools (before this, a session without `--workspace` had none).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_without_a_workspace_the_current_directory_is_the_coding_root() {
    let _serial = pty_guard();
    let dir = TempDir::new("default-workspace");
    let server = MockServer::start(vec![text_reply("OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "default-workspace",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"hi\r");
    assert!(pty.wait_for_screen("OK", 15), "{}", pty.screen());
    let requests = server.requests();
    let tools: Vec<String> = requests[0].json()["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| {
            tool["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert!(tools.iter().any(|name| name == "read"), "{tools:?}");
    assert!(tools.iter().any(|name| name == "write"), "{tools:?}");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `$HOME` itself is never the coding root: the session uses `$HOME/tmp`, creates it `0700`, and
/// says so on the startup line.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_session_started_from_home_uses_home_tmp() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = pty_guard();
    let home = TempDir::new("home-workspace");
    let sessions = TempDir::new("home-workspace-sessions");
    let (mut child, mut master) = spawn_direct_in(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.path().to_str().unwrap(),
            "--session-id",
            "home-workspace",
            "--offline-catalog",
        ],
        home.path(),
        Some(home.path()),
    );
    let needle = "workspace: ~/tmp (cwd is $HOME)";
    // A screen model, because the renderer writes each word at its own cursor position.
    let mut parser = vt100::Parser::new(24, 100, 0);
    let mut buf = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(20);
    while !parser.screen().contents().contains(needle) {
        if let Ok(n) = master.read(&mut buf) {
            parser.process(&buf[..n]);
        }
        assert!(
            Instant::now() < deadline,
            "the startup line is missing:\n{}",
            parser.screen().contents()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let tmp = home.path().join("tmp");
    assert!(tmp.is_dir(), "$HOME/tmp is created");
    let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "owner only");
    drop(master);
    let _ = child.0.kill();
    let _ = child.0.wait();
}

/// The filesystem root is refused, with the way out named.
#[cfg(unix)]
#[test]
fn pty_a_session_started_from_the_filesystem_root_is_refused() {
    let _serial = pty_guard();
    let home = TempDir::new("root-workspace-home");
    let sessions = TempDir::new("root-workspace-sessions");
    let (mut child, mut master) = spawn_direct_in(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.path().to_str().unwrap(),
            "--session-id",
            "root-workspace",
            "--offline-catalog",
        ],
        home.path(),
        Some(std::path::Path::new("/")),
    );
    let mut drained = Vec::new();
    let mut buf = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if let Ok(n) = master.read(&mut buf) {
            drained.extend_from_slice(&buf[..n]);
        }
        assert!(Instant::now() < deadline, "the app did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1), "a startup refusal");
    let text = String::from_utf8_lossy(&drained);
    assert!(text.contains("filesystem root"), "{text}");
    assert!(text.contains("--workspace"), "{text}");
}

/// `--resume` without a value is the picker in "resume only" mode: it lists
/// this workspace's sessions, offers no "new" entry, resumes the chosen one, and Esc leaves
/// without creating a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_resume_without_a_value_picks_a_session() {
    let _serial = pty_guard();
    let dir = TempDir::new("resume-pick");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    seed_session(&sessions, "beta", &workspace, "BETA-HISTORY").await;
    let listing = |sessions: &std::path::Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(sessions)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            // Session files only: the picker's index database is not a session.
            .filter(|name| name.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    };
    let before = listing(&sessions);

    let server = MockServer::start(vec![text_reply("PICKED-REPLY")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--resume",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("alpha", 10),
        "the picker did not list the sessions:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("n new"),
        "a resume-only picker must not offer a new session:\n{}",
        pty.screen()
    );
    // Filter to beta (without it, Enter would resume the first row) and resume it.
    pty.send(b"beta");
    assert!(
        pty.wait_until(5, |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("resumed session beta", 10),
        "Enter did not resume the chosen session:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("BETA-HISTORY", 10),
        "the resumed history is missing:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    // Esc (Quit) leaves the pre-session phase without creating anything.
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--resume",
            "--offline-catalog",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("alpha", 10),
        "the picker did not open again:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(pty.wait_for_exit(10).is_some(), "Esc did not leave");
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(listing(&sessions), before, "no session file was created");
}

/// The exit line is a command the user can paste: the program name, the id,
/// and the flags that must be repeated — and never a key or a header value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_exit_line_is_a_copyable_resume_command() {
    let _serial = pty_guard();
    let dir = TempDir::new("resume-command");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![text_reply("OK")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "copyable",
            "--header",
            "x-trace=HEADER-VALUE",
        ],
        24,
        100,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let after = raw
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    let line = after
        .lines()
        .find(|line| line.contains("resume:"))
        .unwrap_or_else(|| panic!("no resume command on the normal screen: {after}"));
    assert!(line.contains("radian --resume copyable"), "{line}");
    assert!(line.contains("--session-dir"), "{line}");
    assert!(line.contains(sessions.to_str().unwrap()), "{line}");
    assert!(!line.contains("HEADER-VALUE"), "no header value: {line}");
    assert!(!line.contains("MY_KEY"), "no key: {line}");
    assert!(!line.contains("--header"), "{line}");
    assert!(!line.contains("credentials"), "{line}");
}

// ---------- theme files (ADR 0029) ----------

/// A pi theme file under `RADIAN_HOME/themes/` colors the real screen: the transcript's user
/// line renders with the fixture's `userMessageText` RGB (via `userMessageText → var fg`).
/// `COLORTERM=truecolor` makes the app emit RGB, which the vt100 model reports per cell.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_theme_file_colors_the_user_line() {
    let _serial = pty_guard();
    let home = TempDir::new("theme-home");
    // `RADIAN_HOME` root with the theme file and a config selecting it by name.
    let themes = home.path().join("themes");
    std::fs::create_dir_all(&themes).unwrap();
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/themes/kanagawa-wave.json"
    );
    std::fs::copy(fixture, themes.join("kanagawa-wave.json")).unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[tui]\ntheme = \"kanagawa-wave\"\n",
    )
    .unwrap();

    let server = MockServer::start(vec![text_reply("REPLY-DONE")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            home.path().join("sessions").to_str().unwrap(),
            "--session-id",
            "theme",
        ],
        &[
            ("RADIAN_HOME", home.path().to_str().unwrap()),
            ("COLORTERM", "truecolor"),
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"COLOR-PROBE\r");
    assert!(pty.wait_for_screen("REPLY-DONE", 15), "{}", pty.screen());

    // The user entry's `> ` marker carries the theme's `user` style (the body text is the
    // terminal default by design), so check the marker cell: three columns before the probe.
    // The kanagawa fixture's `userMessageText` is `vars.fg = #DCD7BA`.
    let expected = vt100::Color::Rgb(0xDC, 0xD7, 0xBA);
    let screen = pty.parser.screen();
    let mut matched = 0;
    for (row, text) in screen.rows(0, 110).enumerate() {
        for col in text.match_indices("COLOR-PROBE").map(|(i, _)| i) {
            // The input line echoes the probe without a marker; only the transcript entry
            // has "> " before it.
            let Some(marker_col) = (col as u16).checked_sub(2) else {
                continue;
            };
            if let Some(cell) = screen.cell(row as u16, marker_col) {
                assert_eq!(cell.contents(), ">", "expected the user marker");
                assert_eq!(
                    cell.fgcolor(),
                    expected,
                    "the user marker must carry the theme color"
                );
                matched += 1;
            }
        }
    }
    assert!(matched > 0, "the probe was not found on screen");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- /login and /logout (ADR 0026 D3) ----------

/// A small HTTP server that answers every connection with `status` until the process ends
/// (the wizard's verification fires whenever the user submits the key).
fn login_verify_server(status_line: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = "{}";
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://{addr}")
}

/// Pre-seeds `~/.radian/credentials/auth.json` (0700 dir, 0600 file) with one unrelated
/// entry, so the tests can prove it survives a login and a logout.
fn seed_auth_json(home: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join(".radian/credentials");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("auth.json");
    std::fs::write(
        &path,
        r#"{"existing": {"type": "api_key", "key": "$EXISTING_KEY"}}"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn auth_json_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The full custom login over a real pty: name → protocol → base URL → key, the verification
/// request answered by a local server, the save, and the D3 "what next" line. **The key must
/// never appear in any byte the app put on screen** — typed or after the fact — and the
/// pre-existing `auth.json` entry must survive (only `corp` is added, the file still 0600).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_login_custom_walk_saves_without_showing_the_key() {
    let _serial = pty_guard();
    const KEY: &str = "sk-PTY-SECRET-42";
    let verify = login_verify_server("200 OK");
    let dir = TempDir::new("login");

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "login",
        ],
        30,
        110,
    );
    // The app's HOME is the pty's own temp dir; the credentials live there.
    let auth_path = seed_auth_json(pty.home.path());
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());

    pty.send(b"/login custom\r");
    assert!(pty.wait_for_screen("provider name", 5), "{}", pty.screen());
    pty.send(b"corp\r");
    assert!(pty.wait_for_screen("wire protocol", 5), "{}", pty.screen());
    pty.send(b"\r"); // openai-compatible (the highlighted row)
    assert!(pty.wait_for_screen("base URL", 5), "{}", pty.screen());
    pty.send(format!("{verify}\r").as_bytes());
    assert!(pty.wait_for_screen("API key", 5), "{}", pty.screen());
    // Type the key **without submitting** and look at the screen: the field must be masked
    // mid-typing, not only after the wizard closes (a plaintext echo is exactly what D3
    // forbids — this is where a `key.clone()` render or an echo-on terminal would show).
    for c in KEY.chars() {
        pty.send(&[c as u8]);
    }
    // A state gate: the fully typed field is 15 masked columns wide. When the render leaks
    // the plaintext the stars never appear — and the key does, which is what the next
    // assertion is for.
    let stars = "*".repeat(KEY.len());
    assert!(
        pty.wait_until(5, |screen| screen.contains(&stars)),
        "the masked field never filled:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains(KEY),
        "the key echoed onto the screen while typing:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("saved the key for `corp`", 10),
        "{}",
        pty.screen()
    );

    // The key is nowhere on screen, and never was in the byte stream.
    pty.pump();
    let raw_bytes = pty.raw();
    let raw = String::from_utf8_lossy(&raw_bytes);
    assert!(!raw.contains(KEY), "the key leaked into the pty stream");
    assert!(
        !pty.screen().contains(KEY),
        "the key leaked onto the screen:\n{}",
        pty.screen()
    );

    // The record landed in auth.json, next to the untouched entry, still 0600.
    let saved = std::fs::read_to_string(&auth_path).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["corp"]["type"], "api_key");
    assert_eq!(saved["corp"]["key"], KEY);
    assert_eq!(
        saved["corp"]["env"]["RADIAN_BASE_URL"], verify,
        "the custom endpoint rides in the entry's env (D10)"
    );
    assert_eq!(
        saved["existing"]["type"], "api_key",
        "the pre-existing entry survives"
    );
    assert_eq!(auth_json_mode(&auth_path), 0o600);

    // The key reached neither the session journal nor any notice (D3: a key must never be
    // persisted outside auth.json).
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            assert!(!text.contains(KEY), "the key leaked into the journal");
        }
    }

    // The D3 "what next" line names the switch.
    assert!(pty.screen().contains("/model corp/"), "{}", pty.screen());
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The verification failing does not block the save: the wizard shows the reason and `y`
/// stores the key anyway (D3). The same run covers `/logout`: the confirmation, the removal,
/// and the untouched sibling entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_login_failed_verification_can_save_and_logout_removes_only_one() {
    let _serial = pty_guard();
    const KEY: &str = "sk-SECOND-SECRET-7";
    // No server on this port: the verification cannot connect (D3's failure path).
    let dead = "http://127.0.0.1:9/v1";
    let dir = TempDir::new("login-fail");

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "login-fail",
        ],
        30,
        110,
    );
    let auth_path = seed_auth_json(pty.home.path());
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());

    pty.send(b"/login custom\r");
    assert!(pty.wait_for_screen("provider name", 5), "{}", pty.screen());
    pty.send(b"corp\r");
    assert!(pty.wait_for_screen("wire protocol", 5), "{}", pty.screen());
    pty.send(b"\r");
    assert!(pty.wait_for_screen("base URL", 5), "{}", pty.screen());
    pty.send(format!("{dead}\r").as_bytes());
    assert!(pty.wait_for_screen("API key", 5), "{}", pty.screen());
    pty.send(format!("{KEY}\r").as_bytes());
    assert!(
        pty.wait_for_screen("verification failed", 15),
        "{}",
        pty.screen()
    );
    pty.send(b"y"); // save anyway
    assert!(
        pty.wait_for_screen("saved the key for `corp`", 10),
        "{}",
        pty.screen()
    );

    // `/logout corp`: confirm with `y`; the removal names the provider, the other entry and
    // the file mode survive.
    pty.send(b"/logout corp\r");
    assert!(
        pty.wait_for_screen("remove the stored credential for `corp`?", 5),
        "{}",
        pty.screen()
    );
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("removed the stored credential for `corp`", 5),
        "{}",
        pty.screen()
    );

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
    assert!(saved.get("corp").is_none(), "corp was removed: {saved}");
    assert_eq!(saved["existing"]["type"], "api_key", "the sibling survives");
    assert_eq!(auth_json_mode(&auth_path), 0o600);
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `/logout` with `n` keeps the credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_logout_with_n_keeps_the_entry() {
    let _serial = pty_guard();
    let dir = TempDir::new("logout-keep");

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "logout-keep",
        ],
        30,
        110,
    );
    let auth_path = seed_auth_json(pty.home.path());
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"/logout existing\r");
    assert!(
        pty.wait_for_screen("remove the stored credential for `existing`?", 5),
        "{}",
        pty.screen()
    );
    pty.send(b"n");
    assert!(
        pty.wait_for_screen("kept `existing`", 5),
        "{}",
        pty.screen()
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
    assert_eq!(saved["existing"]["type"], "api_key", "kept means kept");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- ADR 0026 D9: the in-session auto-approval switch ----------

/// The `!` key end to end: it approves the pending request once **and** turns auto on for
/// the session, so the next ask never prompts; the status bar says `AUTO`, and both the
/// audit record of the switch and the durable `ConfigChanged` land in the journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_auto_key_approves_once_and_enables_auto() {
    let _serial = pty_guard();
    let dir = TempDir::new("auto-key");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"a.txt","content":"first"}"#),
        tool_reply("call_2", "write", r#"{"path":"b.txt","content":"second"}"#),
        text_reply("SWITCH-DONE"),
    ])
    .await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "auto-key",
            "--workspace",
            ws.to_str().unwrap(),
            // `always` asks on every call, so the second write proves the switch took.
            "--approval",
            "always",
        ],
        30,
        110,
    );
    assert!(pty.wait_for_screen("message", 10), "{}", pty.screen());
    pty.send(b"please write\r");
    assert!(
        pty.wait_for_screen("approve tool call?", 20),
        "no approval prompt:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("AUTO"),
        "auto must be off before the switch:\n{}",
        pty.screen()
    );

    // One press: approve this request and enable auto for the rest of the session.
    pty.send(b"!");
    assert!(
        pty.wait_for_screen("SWITCH-DONE", 30),
        "the turn did not finish after `!`:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("AUTO"),
        "the status bar must say AUTO:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "the second write must not prompt under auto:\n{}",
        pty.screen()
    );
    assert!(
        ws.join("a.txt").exists() && ws.join("b.txt").exists(),
        "both writes must have run: {}",
        pty.screen()
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    let session = std::fs::read_to_string(sessions.join("auto-key.jsonl")).unwrap();
    assert!(
        session.contains("ApprovalPolicyChanged") && session.contains("\"policy\":\"auto\""),
        "the switch must be audited: {session}"
    );
    assert!(
        session.contains("ConfigChanged") && session.contains("\"approval\":\"auto\""),
        "the durable record must be journaled: {session}"
    );
}

/// A journaled in-session switch restores auto on resume under `workspace` (D9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_resume_restores_auto_on_workspace() {
    let _serial = pty_guard();
    let dir = TempDir::new("auto-resume");
    let path = dir.path().join("auto-resume.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "auto-resume",
        radian_session::SessionCreatedData::new("/tmp", json!({})),
    )
    .expect("create the session file");
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("older turn"),
        })
        .await
        .unwrap();
    // What an in-session `/auto on` writes (the driver journals it at the turn boundary).
    journal
        .append_runtime_event(radian_session::RuntimeEvent::ConfigChanged {
            revision: 1,
            change: json!({"approval": "auto"}),
        })
        .await
        .unwrap();
    drop(journal);

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "auto-resume",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("AUTO", 10),
        "the resumed session must show AUTO:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen()
            .contains("auto: tool calls are approved without asking"),
        "the startup line must name the restored policy:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `full` never restores auto (D9): the startup says how to get it back, and the status bar
/// does not show AUTO.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_resume_does_not_restore_auto_on_full_and_hints() {
    let _serial = pty_guard();
    let dir = TempDir::new("auto-resume-full");
    let path = dir.path().join("auto-full.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "auto-full",
        radian_session::SessionCreatedData::new("/tmp", json!({})),
    )
    .expect("create the session file");
    journal
        .append_runtime_event(radian_session::RuntimeEvent::ConfigChanged {
            revision: 1,
            change: json!({"approval": "auto"}),
        })
        .await
        .unwrap();
    drop(journal);

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "auto-full",
            "--profile",
            "full",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen(
            "auto was on in the previous run; run `/auto on` to re-enable",
            10
        ),
        "the full-tier hint is missing:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("AUTO"),
        "full must not restore auto:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Config `[permissions] approval = "auto"` is ignored under `full`, with a warning (D9).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_config_auto_is_ignored_under_full_with_a_warning() {
    let _serial = pty_guard();
    let dir = TempDir::new("auto-config-full");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[permissions]\napproval = \"auto\"\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "config-full",
            "--profile",
            "full",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("ignored under the full tier", 10),
        "the config warning is missing:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("AUTO"),
        "the config value must not enable auto under full:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The same config value holds under `workspace`: auto is on from the first frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_config_auto_enables_auto_on_workspace() {
    let _serial = pty_guard();
    let dir = TempDir::new("auto-config-ws");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[permissions]\napproval = \"auto\"\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut pty = Pty::spawn(
        &[
            "--config",
            config.to_str().unwrap(),
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "config-ws",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("AUTO", 10),
        "the status bar must show AUTO:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// --- transcript selection and copy (mouse drag, OSC 52) ---

/// Finds a needle's `(row, col)` (0-based, display cells) on the vt100 screen.
fn find_on_screen(pty: &mut Pty, needle: &str) -> Option<(u16, u16)> {
    pty.pump();
    let screen = pty.parser.screen();
    let (rows, cols) = screen.size();
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..cols {
            line.push_str(screen.cell(row, col).map(|c| c.contents()).unwrap_or(" "));
        }
        if let Some(col) = line.find(needle) {
            return Some((row, col as u16));
        }
    }
    None
}

/// SGR mouse encoding (`?1006h`): button `code` at 1-based `(col, row)`. `M` presses/moves,
/// `m` releases.
fn sgr(code: u8, col: u16, row: u16, press: bool) -> Vec<u8> {
    format!(
        "\u{1b}[<{code};{col};{row}{}",
        if press { "M" } else { "m" }
    )
    .into_bytes()
}

/// The owner's ask: hold the left button, drag across transcript text, release — the
/// selection's logical text reaches the clipboard as OSC 52 (`ESC]52;c;<base64>BEL`), and the
/// status bar says what actually happened ("via OSC 52"), never that the clipboard holds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_mouse_drag_copies_the_selection_via_osc52() {
    let _serial = pty_guard();
    let dir = TempDir::new("select-copy");
    let path = dir.path().join("select-me.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "select-me",
        radian_session::SessionCreatedData::new("/tmp", json!({})),
    )
    .expect("create the session file");
    use agent_core::TranscriptJournal;
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("SELECT-ME-PLEASE"),
        })
        .await
        .unwrap();
    drop(journal);

    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--offline-catalog",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "select-me",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("SELECT-ME-PLEASE", 10),
        "{}",
        pty.screen()
    );

    // The mouse-reporting switch: button + button-motion + SGR, never any-motion.
    let raw = pty.raw();
    assert!(
        String::from_utf8_lossy(&raw).contains("\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1006h"),
        "the enable sequence must be 1000+1002+1006"
    );
    assert!(
        !String::from_utf8_lossy(&raw).contains("\u{1b}[?1003"),
        "?1003h (any-motion) must stay off: motion without a button would flood the input",
    );

    // Drag across the message's text (the "> " prefix stays out of the copy).
    let (row, col) = find_on_screen(&mut pty, "SELECT-ME-PLEASE").expect("the text on screen");
    let end = col + "SELECT-ME-PLEASE".len() as u16 - 1;
    pty.send(&sgr(0, col + 1, row + 1, true));
    pty.send(&sgr(32, end + 1, row + 1, true));
    pty.send(&sgr(0, end + 1, row + 1, false));

    // The copy: OSC 52 with exactly the selected text, and the honest notice.
    let expected = radian_tui::clip::osc52("SELECT-ME-PLEASE");
    assert!(
        pty.wait_for_raw(&expected, 10),
        "no OSC 52 with the selection: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("copied 16 chars (via OSC 52)", 10),
        "the status bar must say what happened, not claim the clipboard:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// Motion without a button (`?1002h` does not report it; `?1003h` stays off) produces no
/// events, no wake-ups and no frames (ADR 0023 U2's zero-redraw, pointer edition).
#[test]
fn pty_pointer_motion_alone_draws_no_frame() {
    let _serial = pty_guard();
    let dir = TempDir::new("motion-idle");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "motion-idle",
            "--offline-catalog",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", 10),
        "{}",
        pty.screen()
    );
    let before = frame_count(&pty);

    // A pointer wandering across the screen, buttons up, for a while.
    for step in 0..40u16 {
        pty.send(&sgr(35, 5 + step, 3 + step % 10, true));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let after = frame_count(&pty);
    pty.quit();
    assert_eq!(
        after, before,
        "pointer motion without a button must not wake the loop"
    );
}

/// Every exit path turns mouse reporting back off, `?1002l` included (the drag tracking this
/// slice added). The ordinary path: the guard's drop.
#[test]
fn pty_quit_closes_button_motion_reporting() {
    let _serial = pty_guard();
    let dir = TempDir::new("select-exit");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "select-exit",
            "--offline-catalog",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", 10),
        "{}",
        pty.screen()
    );
    pty.quit();
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains("\u{1b}[?1006l\u{1b}[?1002l\u{1b}[?1000l"),
        "the restore must close 1006, 1002 and 1000 together: {raw:?}"
    );
}

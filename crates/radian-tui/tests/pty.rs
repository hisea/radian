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
    cmd.env("RADIAN_KEYBOARD_ENHANCEMENT", "0");
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

/// The positive deadline: how long a wait for something to **appear**, to **change**, or for the
/// app to **exit** may take before the test fails. Every positive wait in this file uses it (via
/// [`wait_positive`]); a wait that measures a bound on purpose passes its own constant instead
/// (the retry probes, [`WAIT_LATENCY_BOUND`]).
///
/// 60 s: the gate runs this suite at load average 70-200 beside several cargo builds, where the
/// 10-20 s literals it used to carry flaked (2026-10-16/17; `pty_herdr_reports_the_turn_states_
/// and_releases` did not see `working` within 10 s). 60 s is three times the largest of them and
/// six times the flaked one, and a passing run pays nothing for the headroom: every helper polls
/// every 20 ms and returns as soon as its condition holds, so only a failing wait sits it out.
const WAIT_POSITIVE: Duration = Duration::from_secs(60);

/// [`WAIT_POSITIVE`], raised for one run with `RADIAN_TEST_WAIT_SECS` (a slower machine, or a
/// soak run, without an edit; `RADIAN_PTY_PARALLEL` is the sibling knob). An unparsable value is
/// ignored, so a typo cannot silently shorten a deadline.
fn wait_positive() -> Duration {
    std::env::var("RADIAN_TEST_WAIT_SECS")
        .ok()
        .and_then(|secs| secs.trim().parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(WAIT_POSITIVE)
}

/// The window for a **negative** check ("X must not appear"): short on purpose, because here the
/// wait *is* the assertion, so a longer window only makes every run slower.
const WAIT_NEGATIVE: Duration = Duration::from_secs(2);

/// A deliberate latency bound: the test asserts that something arrives **fast**, so the wait is
/// the measurement and stays short. `pty_attach_follow_shows_new_committed_events` pins the
/// attach follow to "within a second" (the README hand-test note).
const WAIT_LATENCY_BOUND: Duration = Duration::from_secs(1);

/// ADR 0016: a test that depends on **sandbox behaviour** cannot run
/// inside another sandbox. The nested `sandbox-exec` a probe starts is refused by the kernel
/// (`sandbox_apply: Operation not permitted`), and what the outer sandbox allows is unknown, so
/// the test prints a fixed marker and returns. `cargo` still counts it as passed: the verify
/// output and every report must list the marker lines as **not run**, never as passing.
macro_rules! skip_inside_sandbox {
    ($name:literal) => {
        if radian_sandbox::confined_by_outer_sandbox() {
            println!("radian-test: skipped inside a sandbox: {}", $name);
            return;
        }
    };
}

/// The pty tests spawn real processes and mock servers. Running all of them at once makes
/// them flake on a busy machine, and running them one at a time made this binary about 140 s
/// of the workspace test run (2026-10-15). A small counting gate allows a few at once:
/// `RADIAN_PTY_PARALLEL` (default 3, minimum 1) sets how many.
static PTY_SLOTS: Mutex<usize> = Mutex::new(0);
static PTY_FREED: std::sync::Condvar = std::sync::Condvar::new();

fn pty_parallelism() -> usize {
    std::env::var("RADIAN_PTY_PARALLEL")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3)
        .max(1)
}

/// One running pty test; the slot is given back on drop (a panicking test included).
struct PtySlot;

impl Drop for PtySlot {
    fn drop(&mut self) {
        let mut busy = PTY_SLOTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *busy -= 1;
        PTY_FREED.notify_one();
    }
}

fn pty_guard() -> PtySlot {
    let limit = pty_parallelism();
    let mut busy = PTY_SLOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while *busy >= limit {
        busy = PTY_FREED
            .wait(busy)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    *busy += 1;
    PtySlot
}

struct Pty {
    writer: Box<dyn Write + Send>,
    bytes: Arc<Mutex<Vec<u8>>>,
    parser: Parser,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The pty master, kept so a test can change the window size (`TIOCSWINSZ`) without
    /// sending any input (the owner's bare-resize bug).
    master: Box<dyn portable_pty::MasterPty + Send>,
    processed: usize,
    /// A private `HOME` for the app: its catalog cache lives here, never in the user's real
    /// cache. Held only so the directory is removed when the pty is dropped.
    #[allow(dead_code)]
    home: TempDir,
}

/// What config file a spawned test gets (ADR 0027 C2): the suite's existing assertions were
/// written for the pre-ADR one-line status bar, so most tests get `preset = classic`; a test
/// that wants the real no-config default uses [`Pty::spawn_bare`].
enum TestConfig<'a> {
    /// Write `preset = "classic"` (the suite's default).
    Classic,
    /// Write this config file.
    Explicit(&'a str),
    /// Write nothing: the app starts on its real defaults.
    Absent,
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
        Self::spawn_full_mode(args, envs, TestConfig::Classic, rows, cols)
    }

    /// Like [`Pty::spawn_with_env`], with the app started **from** `cwd`: a test that depends on
    /// the current directory (the default workspace, `attach`'s picker) sets it here.
    fn spawn_in(args: &[&str], envs: &[(&str, &str)], cwd: &Path, rows: u16, cols: u16) -> Self {
        let mut argv = vec![BIN.to_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        Self::spawn_argv_in(&argv, envs, TestConfig::Classic, Some(cwd), rows, cols)
    }

    /// Like [`Pty::spawn`], writing **no** config file: the app runs its real defaults (the
    /// two-row `pi` footer, ADR 0027 C2). A test about the shipped default must use this.
    fn spawn_bare(args: &[&str], envs: &[(&str, &str)], rows: u16, cols: u16) -> Self {
        Self::spawn_full_mode(args, envs, TestConfig::Absent, rows, cols)
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
        Self::spawn_full_mode(args, envs, TestConfig::Explicit(config), rows, cols)
    }

    fn spawn_full_mode(
        args: &[&str],
        envs: &[(&str, &str)],
        config: TestConfig<'_>,
        rows: u16,
        cols: u16,
    ) -> Self {
        let mut argv = vec![BIN.to_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        Self::spawn_argv(&argv, envs, config, rows, cols)
    }

    /// Like [`Pty::spawn_full_mode`], with the whole command line given: `argv[0]` is the
    /// program the pty's shell runs and the rest are its arguments. ADR 0016
    /// acceptance 2 starts `radian` under `sandbox-exec`, which needs a launcher in front of
    /// the bin.
    fn spawn_argv(
        argv: &[String],
        envs: &[(&str, &str)],
        config: TestConfig<'_>,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self::spawn_argv_in(argv, envs, config, None, rows, cols)
    }

    /// [`Pty::spawn_argv`], with the pty's shell started in `cwd` (the app inherits it).
    fn spawn_argv_in(
        argv: &[String],
        envs: &[(&str, &str)],
        config: TestConfig<'_>,
        cwd: Option<&Path>,
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
        for arg in argv {
            cmd.arg(arg);
        }
        // The shell (and so the app) starts in `cwd`: the current directory is the workspace an
        // app derives from it (`attach`'s picker, the default workspace).
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
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
        // The footer's default is the two-row `pi` layout (ADR 0027 C2). The suite's existing
        // assertions were written for the pre-ADR one-line status bar, so a test that does not
        // bring its own config gets `preset = "classic"`; `spawn_bare` writes nothing and sees
        // the shipped default.
        match config {
            TestConfig::Absent => {}
            other => {
                let dir = auth_root.join("config");
                std::fs::create_dir_all(&dir).unwrap();
                let text = match other {
                    TestConfig::Explicit(text) => text.to_owned(),
                    _ => "schema_version = 1\n[tui.footer]\npreset = \"classic\"\n".to_string(),
                };
                std::fs::write(dir.join("config.toml"), text).unwrap();
            }
        }
        // `cmd.env_clear()` above is what makes this hermetic: neither `RADIAN_HOME` nor
        // `RADIAN_CONFIG` reaches the child, so a config can only come from this temp `HOME`.
        cmd.env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json");
        // A pty has nobody to answer the kitty keyboard-enhancement query, and crossterm
        // waits up to 2 s for it; the harness opts out (a test that wants the sequences sets
        // the variable back to `1` through `envs`).
        cmd.env("RADIAN_KEYBOARD_ENHANCEMENT", "0");
        for (name, value) in envs {
            cmd.env(name, value);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let master = pair.master;
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
            master,
            processed: 0,
            home,
        }
    }

    /// Changes the pty's window size **without sending any input**: a `TIOCSWINSZ` on the
    /// master, which is what dragging a window does. The app must re-lay out on its own.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
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
    fn wait_for_screen(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
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

    /// A **negative** check: `needle` must not show up on the screen. The wait *is* the
    /// assertion ([`WAIT_NEGATIVE`]), so it stays short; returns true when it stayed absent.
    fn screen_absent(&mut self, needle: &str) -> bool {
        !self.wait_for_screen(needle, WAIT_NEGATIVE)
    }

    /// Waits until `predicate` holds for the current screen contents. The predicate is the
    /// synchronization; the deadline only bounds a failure. A predicate that waits for something
    /// to **leave** the screen (`!screen.contains(..)`) is still a positive wait, not a negative
    /// check: it returns as soon as the transition happened.
    fn wait_until(&mut self, timeout: Duration, predicate: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
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
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<u32> {
        let deadline = Instant::now() + timeout;
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
    /// Expands the startup information block (Ctrl-P). the block is folded
    /// by default, so a test that asserts one of the full startup lines expands it first. A
    /// lost keypress is retried; the wait after each send means a successful toggle is never
    /// pressed twice. One attempt's 1 s is the loop's rhythm, not a failure bound.
    fn expand_startup(&mut self) {
        for _ in 0..3 {
            self.send(&[0x10]); // ctrl-p
            if self.wait_until(Duration::from_secs(1), |screen| {
                // The expanded header and the first info line are at the **top** of the block;
                // with the trust line the block can be taller than a 24-row pty, so accept a
                // later info line that stays visible when the view is pinned to the bottom.
                screen.contains("- startup information")
                    || screen.contains("profile:")
                    || screen.contains("ask_user:")
            }) {
                return;
            }
        }
        panic!("the startup block did not expand:\n{}", self.screen());
    }

    fn quit(&mut self) {
        // The first idle Ctrl-C arms the 2 s quit window and the second inside it quits
        //, so a quit is a **pair** of presses; a running turn needs a
        // cancel first, which the retries cover (the next pair then arms and quits). One pair's
        // 3 s is the loop's rhythm, not a failure bound.
        for _ in 0..4 {
            self.send(&[0x03]);
            self.send(&[0x03]);
            if self.wait_for_exit(Duration::from_secs(3)).is_some() {
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
    fn wait_for_raw(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
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

    /// Waits until `needle` is **gone** from the screen (a modal that was cleared). Like a
    /// `!contains` predicate, this is a positive wait for a transition, not a negative check.
    fn wait_until_gone(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
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
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"hello from the test\r");
    assert!(
        pty.wait_for_screen("ASSISTANT-REPLY-OK", wait_positive()),
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

/// ADR 0043: a fresh TUI session mounts only the built-in tools. No `git_commit` and
/// none of the deleted task/lead/named tools are advertised to the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_mounts_only_the_builtin_tools() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("TASKS-REPLY-OK")]).await;
    let dir = TempDir::new("tasks");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "tasks",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("TASKS-REPLY-OK", wait_positive()),
        "reply not rendered:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    let names: Vec<String> = server.requests()[0].json()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    // The deleted surface: `git_commit` and every task/lead/named tool are gone.
    assert!(
        !names.iter().any(|name| name == "git_commit"),
        "`git_commit` must not be mounted: {names:?}"
    );
    for gone in [
        "spawn_task",
        "wait_task",
        "wait_any",
        "list_tasks",
        "task_status",
        "cancel_task",
        "remove_task",
        "integrate_task",
        "answer_task",
        "deny_task_approval",
        "escalate_approval",
        "send_agent",
        "wait_agent",
    ] {
        assert!(
            !names.iter().any(|name| name == gone),
            "`{gone}` must not be mounted: {names:?}"
        );
    }
    // The built-ins a workspace session mounts are exactly these, and nothing else. The
    // removed names (`git_diff`, `list`, `git_status`, `job_output`, `job_list`,
    // `job_kill`, `shell_output`) are never advertised: a call to one is an unknown tool.
    // `memory_search` mounts only past the injection budget (ADR 0021 D7),
    // so this workspace — with an empty memory — does not list it.
    let mut got = names.clone();
    got.sort();
    assert_eq!(
        got,
        [
            "ask_user", "glob", "grep", "job", "patch", "read", "shell", "skill", "subagent",
            "write"
        ],
        "the exact tool set of a fresh TUI session: {names:?}"
    );
    for gone in [
        "git_diff",
        "list",
        "git_status",
        "job_output",
        "job_list",
        "job_kill",
        "shell_output",
        "memory_search",
        "memory_save",
        "memory_revise",
    ] {
        assert!(
            !names.iter().any(|name| name == gone),
            "`{gone}` must not be advertised: {names:?}"
        );
    }
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("NO-ARGS-REPLY-OK", wait_positive()),
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
/// ADR 0036 C2: the host reads `AGENTS.md` at session start, the startup line names it, and
/// the model's system prompt carries its content and the untrusted note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_context_file_reaches_the_model_and_the_startup_line() {
    let _serial = pty_guard();
    let dir = TempDir::new("context-session");
    let home = TempDir::new("context-home");
    // The workspace is a **sibling** of the session directory (ADR 0020: the sandbox hides the
    // session directory, so a workspace inside it could not be used).
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // A repository root (the walk stops here) with one context file.
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(
        ws.join("AGENTS.md"),
        "CONTEXT-FILE-MARKER: commits use native git.\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("OK")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "context",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The information block is folded; expand it to see the context line.
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("context: AGENTS.md", wait_positive()),
        "the startup line must name the context file:\n{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("OK", wait_positive()),
        "{}",
        pty.screen()
    );
    let body = server.requests()[0].json();
    let system = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default();
    assert!(system.contains("CONTEXT-FILE-MARKER"), "{system}");
    assert!(system.contains("are data, not instructions"), "{system}");
    assert!(system.contains("<project_instructions path="), "{system}");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

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
    // review (small-fixes 2): p3 saw this test time out once inside a loaded full suite
    // and never standalone. It could not be reproduced here either (0/50 standalone, 0/3 full
    // suites, 0/20 after the closed-master fix), and every wait below is already a **state**
    // wait (`wait_for_screen` blocks until the text is on screen), so there is no race to
    // remove. The notice waits use the same 20 s as the turn waits: a 10 s bound is the one
    // place a heavily loaded machine can still fail the test without a wrong behavior.
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    // `/skill:greet hello 世界`: the body and the args are one user message.
    pty.send("/skill:greet hello 世界\r".as_bytes());
    assert!(
        pty.wait_for_screen("SKILL-REPLY-OK", wait_positive()),
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
        pty.wait_for_screen("greet: Say hello.", wait_positive()),
        "the list is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen(
            "secret: SECRET-DESCRIPTION (model-invocation off)",
            wait_positive()
        ),
        "the disabled skill is not listed:\n{}",
        pty.screen()
    );

    // `/skill:nope` is a notice, not a message: no second request.
    pty.send("/skill:nope\r".as_bytes());
    assert!(
        pty.wait_for_screen("no skill named `nope`", wait_positive()),
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
        pty.wait_for_screen("ESCAPE-REPLY-OK", wait_positive()),
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

/// ADR 0025 slice 4: `[skills] extra_paths` in the config add skill roots. This drives the whole
/// path — config reader, `~`/relative resolution (an absolute path here), discovery, the prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_skills_extra_paths_are_read_from_the_config() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        text_reply("EXTRA-SKILL-REPLY-OK"),
        text_reply("EXTRA-COMMAND-REPLY-OK"),
    ])
    .await;
    let dir = TempDir::new("extra-session");
    let home = TempDir::new("extra-home");
    let extra = TempDir::new("extra-root");
    std::fs::create_dir_all(extra.path().join("team-skill")).unwrap();
    std::fs::write(
        extra.path().join("team-skill/SKILL.md"),
        "---\nname: team-skill\ndescription: From the extra root.\n---\nEXTRA-ROOT-BODY\n",
    )
    .unwrap();
    let config = format!(
        "schema_version = 1\n[skills]\nextra_paths = [\"{}\"]\n",
        extra.path().to_str().unwrap()
    );
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "extra",
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        &config,
        24,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send("hello\r".as_bytes());
    assert!(
        pty.wait_for_screen("EXTRA-SKILL-REPLY-OK", wait_positive()),
        "the reply is missing:\n{}",
        pty.screen()
    );
    let body = server.requests()[0].json();
    let system = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default();
    assert!(system.contains("team-skill"), "{system}");
    assert!(system.contains("From the extra root."), "{system}");
    // The extra skill is usable end to end through the command path too.
    pty.send("/skill:team-skill\r".as_bytes());
    assert!(
        pty.wait_for_screen("EXTRA-COMMAND-REPLY-OK", wait_positive()),
        "the extra skill got no reply:\n{}",
        pty.screen()
    );
    let second = server.requests()[1].json();
    let expanded = second["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(expanded.contains("EXTRA-ROOT-BODY"), "{expanded}");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit code");
}

/// ADR 0031 T3: a workspace with project resources and no saved decision shows the startup
/// trust modal. The default highlight is the sandbox choice,
/// so this test moves down to "do not trust this directory", writes `trust.json` with
/// Enter, and journals `ProjectTrustChanged`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_trust_modal_asks_and_enter_untrusts() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("TRUST-REPLY-OK")]).await;
    let dir = TempDir::new("trust-session");
    let home = TempDir::new("trust-home");
    let ws = TempDir::new("trust-ws");
    std::fs::create_dir_all(ws.path().join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.path().join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: d\n---\nbody\n",
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
            "trust",
            "--workspace",
            ws.path().to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        30,
        120,
    );
    assert!(
        pty.wait_for_screen("project trust", wait_positive()),
        "the trust modal is missing:\n{}",
        pty.screen()
    );
    // All six options, sandbox first; the default highlight is the sandbox choice.
    assert!(
        pty.wait_for_screen("Trust this directory, and use the sandbox", wait_positive()),
        "the sandbox choice is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("Trust the parent", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("Trust this session only", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("Do not trust this directory", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("Do not trust, this session only", wait_positive()),
        "{}",
        pty.screen()
    );

    // Down four rows from the sandbox default to "do not trust this directory", then Enter.
    for _ in 0..4 {
        pty.send(b"\x1b[B"); // Down
    }
    pty.send(b"\r");
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("project resources: not loaded", wait_positive()),
        "the trust state was not shown:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    // `trust.json` holds the workspace → false (canonicalized).
    let canonical = std::fs::canonicalize(ws.path()).unwrap();
    let trust = std::fs::read_to_string(home.path().join("trust/trust.json")).unwrap();
    assert!(
        trust.contains(&canonical.to_string_lossy().to_string()),
        "{trust}"
    );
    assert!(trust.contains("false"), "{trust}");
    // The write is audited in the session journal (ADR 0031 T2/T6).
    let journal = std::fs::read_to_string(dir.path().join("trust.jsonl")).unwrap();
    assert!(journal.contains("ProjectTrustChanged"), "{journal}");
}

/// ADR 0043 decision 2 (revised 2026-10-20): the trust prompt's sandbox choice is first
/// and pre-selected, so Enter trusts the directory **and** remembers `workspace` for it
/// in `access.json`, the same record `/access` writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_trust_sandbox_choice_writes_the_access_record() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("TRUST-SANDBOX-OK")]).await;
    let dir = TempDir::new("trust-sandbox-session");
    let home = TempDir::new("trust-sandbox-home");
    let ws = TempDir::new("trust-sandbox-ws");
    std::fs::create_dir_all(ws.path().join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.path().join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: d\n---\nbody\n",
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
            "trust-sandbox",
            "--workspace",
            ws.path().to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        30,
        120,
    );
    assert!(
        pty.wait_for_screen("project trust", wait_positive()),
        "the trust modal is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("use the sandbox for this project", wait_positive()),
        "{}",
        pty.screen()
    );
    // The sandbox choice is first and pre-selected: Enter takes it directly.
    pty.send(b"\r");
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("project resources: loaded", wait_positive()),
        "the trust state was not shown:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    let canonical = std::fs::canonicalize(ws.path()).unwrap();
    // Trust is recorded…
    let trust = std::fs::read_to_string(home.path().join("trust/trust.json")).unwrap();
    assert!(
        trust.contains(&canonical.to_string_lossy().to_string()) && trust.contains("true"),
        "{trust}"
    );
    // …and the sandbox is remembered for the project, keyed by the project identity.
    let access = std::fs::read_to_string(home.path().join("access/access.json")).unwrap();
    assert!(
        access.contains(&canonical.to_string_lossy().to_string()),
        "{access}"
    );
    assert!(access.contains("workspace"), "{access}");
}

/// ADR 0031 T3: `--trust` overrides for this process only and never writes `trust.json`; a
/// saved decision (and a bare `.radian/`) means no question.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_trust_override_and_saved_decision_skip_the_modal() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![]).await;
    let dir = TempDir::new("trust-session-2");
    let home = TempDir::new("trust-home-2");
    let ws = TempDir::new("trust-ws-2");
    std::fs::create_dir_all(ws.path().join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.path().join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: d\n---\nbody\n",
    )
    .unwrap();
    let args = |id: &str, extra: &[&str]| -> Vec<String> {
        let mut args: Vec<String> = vec![
            "--model".into(),
            "mock".into(),
            "--base-url".into(),
            server.base_url.clone(),
            "--session-dir".into(),
            dir.path().to_str().unwrap().into(),
            "--session-id".into(),
            id.into(),
            "--workspace".into(),
            ws.path().to_str().unwrap().into(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args
    };
    // `--trust`: no modal, no file.
    let owned = args("trust-a", &["--trust"]);
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut pty = Pty::spawn_with_env(
        &refs,
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        24,
        110,
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("project resources: loaded", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(!pty.screen().contains("project trust"), "{}", pty.screen());
    pty.quit();
    assert!(
        !home.path().join("trust/trust.json").exists(),
        "`--trust` must not write"
    );

    // A saved decision: no modal, the saved source is named.
    let canonical = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::create_dir_all(home.path().join("trust")).unwrap();
    std::fs::write(
        home.path().join("trust/trust.json"),
        format!("{{\"{}\": true}}", canonical.display()),
    )
    .unwrap();
    let owned = args("trust-b", &[]);
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut pty = Pty::spawn_with_env(
        &refs,
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        24,
        110,
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen(
            "project resources: loaded (trusted: saved for",
            wait_positive()
        ),
        "{}",
        pty.screen()
    );
    assert!(!pty.screen().contains("project trust"), "{}", pty.screen());
    pty.quit();

    // A bare `.radian/` does not trigger.
    let bare = TempDir::new("trust-bare");
    std::fs::create_dir_all(bare.path().join(".radian")).unwrap();
    let home3 = TempDir::new("trust-home-3");
    let owned = vec![
        "--model".to_owned(),
        "mock".to_owned(),
        "--base-url".to_owned(),
        server.base_url.clone(),
        "--session-dir".to_owned(),
        dir.path().to_str().unwrap().into(),
        "--session-id".to_owned(),
        "trust3".to_owned(),
        "--workspace".to_owned(),
        bare.path().to_str().unwrap().into(),
    ];
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let mut pty = Pty::spawn_with_env(
        &refs,
        &[("RADIAN_HOME", home3.path().to_str().unwrap())],
        24,
        110,
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen(
            "project resources: not loaded (untrusted: default)",
            wait_positive()
        ),
        "{}",
        pty.screen()
    );
    assert!(!pty.screen().contains("project trust"), "{}", pty.screen());
    pty.quit();
}

/// decision 2026-10-20: a project with no trusted decision shows
/// `untrusted project · /trust to trust it` under the wordmark (no modal when there
/// are no project resources); a trusted project shows no hint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_startup_shows_the_trust_hint_until_trusted() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("HINT-OK")]).await;
    let dir = TempDir::new("trust-hint-session");
    let home = TempDir::new("trust-hint-home");
    let ws = TempDir::new("trust-hint-ws");
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "trust-hint",
            "--workspace",
            ws.path().to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        30,
        120,
    );
    assert!(
        pty.wait_for_screen("/trust to trust it", wait_positive()),
        "the trust hint is missing:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    // `--trust` trusts for this process: the expanded block loads, and the hint is gone.
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "trust-hint-trusted",
            "--workspace",
            ws.path().to_str().unwrap(),
            "--trust",
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        30,
        120,
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("project resources: loaded", wait_positive()),
        "the session did not start:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("/trust to trust it"),
        "a trusted project shows no hint:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit"
    );
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
        gone_within(pid, wait_positive()),
        "the child {pid} outlived the panic"
    );
    assert!(
        !group_alive(pid),
        "the child's process group {pid} still has members"
    );
}

/// The panic path must not orphan a **background job**: its whole process group is killed
/// (ADR 0037 decision 6, `Jobs::drop` -> `kill_all`, killpg only). The job writes its pid
/// before the app panics.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_panic_path_does_not_orphan_a_background_job() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-job");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
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
            "--panic-with-background-job",
            "echo $$ > job.pid; sleep 600",
        ],
        20,
        80,
    );
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit after the panic"
    );
    assert_eq!(
        pty.app_exit_code(),
        Some(101),
        "panic must exit 101: {}",
        String::from_utf8_lossy(&pty.raw())
    );

    let pid: i32 = std::fs::read_to_string(ws.join("job.pid"))
        .expect("the app wrote job.pid before panicking")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        gone_within(pid, wait_positive()),
        "the background job (pid {pid}) outlived the panic"
    );
    assert!(
        !group_alive(pid),
        "the background job's process group {pid} still has members"
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
        pty.wait_for_screen("OLD-USER-TURN", wait_positive()),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("OLD-ASSISTANT-REPLY", wait_positive()),
        "the assistant reply is missing:\n{}",
        pty.screen()
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("resumed session resume-me", wait_positive()),
        "the resume notice is missing:\n{}",
        pty.screen()
    );

    // The resumed session keeps appending to the same file.
    pty.send(b"continue\r");
    assert!(
        pty.wait_for_screen("new reply", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"please write\r");

    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
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
        pty.wait_for_screen("WROTE-IT", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "no prompt:\n{}",
        pty.screen()
    );
    pty.send(b"n");
    // The reply and the denial are two different events: wait for both, so a frame that shows
    // the reply before the tool result's line cannot make this flake.
    assert!(
        pty.wait_until(wait_positive(), |screen| {
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "no first prompt:\n{}",
        pty.screen()
    );
    pty.send(b"a");
    // No further key press: if the second call asked again the turn would stall here.
    assert!(
        pty.wait_for_screen("DONE-TWICE", wait_positive()),
        "the session grant did not cover the second call:\n{}",
        pty.screen()
    );
    let written = std::fs::read_to_string(ws.join("same.txt")).unwrap();
    assert_eq!(written, "two", "the second write did not run");

    pty.quit();
}

/// The owner's exact report: a tool is running (the input is empty), Esc must cancel the
/// turn. The command runs long, so the turn is still running when Esc arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_escape_cancels_a_running_tool_turn() {
    let _serial = pty_guard();
    let dir = TempDir::new("escape-tool");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![tool_reply(
        "call_1",
        "shell",
        &json!({"command": "sleep 120"}).to_string(),
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
            "escape-tool",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "auto",
        ],
        30,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("sleep 120", wait_positive()),
        "the tool did not start:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_screen("cancelled", wait_positive()),
        "Esc did not cancel the turn:\n{}",
        pty.screen()
    );
}

/// with an empty input and a turn running, Esc must cancel the turn (the
/// README says so). The dogfood build (2b55c50) did not; this pins the behaviour. The reply
/// never finishes, so the turn stays running until the cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_escape_cancels_a_running_turn() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let dir = TempDir::new("escape-cancel");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "escape-cancel",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    // Wait until the request reached the mock: the turn is running now.
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.requests().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !server.requests().is_empty(),
        "the turn never started:\n{}",
        pty.screen()
    );
    // The input is empty (the message was sent): Esc cancels the running turn.
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_screen("cancelled", wait_positive()),
        "Esc did not cancel the turn:\n{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "no prompt:\n{}",
        pty.screen()
    );

    pty.send(&[0x03]); // Ctrl-C: cancel the turn, not answer the prompt
    assert!(
        pty.wait_for_screen("cancelled", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // ~1000 estimated tokens, over B = 750.
    pty.send(format!("\x1b[200~{}\x1b[201~\r", "x".repeat(4_000)).as_bytes());
    assert!(
        pty.wait_for_screen("OVER-BUDGET-REPLY", wait_positive()),
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
    // The assertion is about the pre-ADR one-line status bar (ADR 0027 C2 moved the context
    // text into the itemized footer), so this runs the `classic` preset.
    let config = dir.path().join("classic.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[tui.footer]\npreset = \"classic\"\n",
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
            "budget-off",
        ],
        24,
        110,
    );
    assert!(
        pty.wait_for_screen("context budget off", wait_positive()),
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
    skip_inside_sandbox!("pty_a_failed_sandbox_probe_refuses_instead_of_running_unsandboxed");
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
            "--profile",
            "workspace",
        ],
        &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        24,
        100,
    );
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit after the probe failed"
    );
    assert_eq!(
        pty.app_exit_code(),
        Some(2),
        "a startup refusal is a usage/setup error (ADR 0032 A3)"
    );
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

/// ADR 0016 acceptance 2: under a deny-default outer sandbox `radian` does **not**
/// refuse. It inherits the outer sandbox, the startup line says so, and the constraint it
/// reports is really the outer one's: a write outside the profile's writable subtree fails, one
/// inside succeeds. The profile is the smallest that lets the TUI start (measured on macOS 27,
/// 2026-10-16); `(deny default)` is what makes the nested `sandbox-exec` probe impossible.
///
/// The test is not skipped inside another sandbox: a nested `sandbox-exec` cannot start there
/// (that is the whole point), so it runs the bin directly and checks the one thing still
/// checkable — that the outer sandbox **this test's own process** is in is inherited and named.
/// The write checks need a profile whose writable subtree is known, so they run only outside.
///
/// macOS-only: it wraps the bin in `sandbox-exec`, the one outer sandbox this repository knows
/// how to build. Linux has no `sandbox-exec`, and the `inherited` backend is unreachable there —
/// `confined_by_outer_sandbox()` answers `false` on every non-macOS target (`radian-sandbox/src/
/// lib.rs`), because bwrap nesting and a kernel "already confined" signal are unverified on Linux
/// (ADR 0016). A Linux twin would have nothing to inherit, so the test is
/// compiled out there rather than failing on the missing binary.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_an_outer_sandbox_is_inherited_instead_of_refused() {
    let _serial = pty_guard();
    let dir = TempDir::new("inherited");
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // `HOME`, `TMPDIR` and the session directory live inside the one writable subtree, so the
    // profile needs a single write rule and `outside/` is really outside it.
    let home = ws.join("home");
    let tmp = ws.join("tmp");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let leak = outside.join("leak");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let outer = !radian_sandbox::confined_by_outer_sandbox();
    let mut argv: Vec<String> = Vec::new();
    if outer {
        argv.extend([
            SANDBOX_EXEC.to_owned(),
            "-p".to_owned(),
            outer_profile(&ws),
            "--".to_owned(),
        ]);
    }
    argv.push(BIN.to_owned());
    argv.extend(
        [
            "--model",
            "mock",
            "--base-url",
            server.base_url.as_str(),
            "--session-dir",
            ws.join("sessions").to_str().unwrap(),
            "--session-id",
            "inherited",
            "--workspace",
            ws.to_str().unwrap(),
            // ADR 0043: the inherited backend belongs to the `workspace` tier, which is no
            // longer the default.
            "--profile",
            "workspace",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    let mut pty = Pty::spawn_argv(
        &argv,
        &[
            ("HOME", home.to_str().unwrap()),
            ("TMPDIR", tmp.to_str().unwrap()),
        ],
        TestConfig::Classic,
        30,
        110,
    );
    // No refusal: the composer is up.
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "the app refused to start under the outer sandbox:\n{}",
        pty.screen()
    );
    pty.expand_startup();
    let startup = pty
        .screen()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        startup.contains("sandbox inherited"),
        "the startup line must say the sandbox is inherited:\n{startup}"
    );

    if outer {
        // A write outside the writable subtree is denied by the **outer** sandbox (the `!`
        // command reports the shell's exit status through a file it is allowed to write).
        pty.send(
            format!(
                "!printf x > {}; echo $? > ./outside_status\r",
                leak.display()
            )
            .as_bytes(),
        );
        let status = ws.join("outside_status");
        assert!(
            wait_for_bang_file(&mut pty, &status),
            "the `!` command outside the workspace did not finish:\n{}",
            pty.screen()
        );
        let code = std::fs::read_to_string(&status).unwrap();
        assert_ne!(
            code.trim(),
            "0",
            "the outer sandbox must deny a write outside the writable subtree"
        );
        assert!(!leak.exists(), "a file outside the workspace was created");

        // The same command inside the writable subtree succeeds.
        pty.send(b"!printf x > ./inside\r");
        let inside = ws.join("inside");
        assert!(
            wait_for_bang_file(&mut pty, &inside),
            "the `!` command inside the workspace did not finish:\n{}",
            pty.screen()
        );
    }

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The smallest `(deny default)` profile under which the TUI starts (ADR 0016
/// acceptance 2, measured on macOS 27, 2026-10-16). Reads are open — the point is the write
/// rule and the deny default — and writes are limited to `ws`, which holds the workspace, the
/// session file, `HOME` and `TMPDIR`. `process*` is what lets `sandbox-exec` exec the bin (and
/// what a nested `sandbox-exec` would need in vain); the Mach/sysctl/IPC/signal allows are what
/// the Rust runtime and tokio need to come up under `(deny default)`.
#[cfg(target_os = "macos")]
fn outer_profile(ws: &Path) -> String {
    let ws = std::fs::canonicalize(ws).unwrap();
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow file-read*)\n\
         (allow process*)\n\
         (allow sysctl*)\n\
         (allow mach*)\n\
         (allow ipc*)\n\
         (allow system*)\n\
         (allow signal)\n\
         (allow file-ioctl)\n\
         (allow file-write* (subpath \"{}\"))\n",
        ws.display()
    )
}

/// Waits (bounded) for a file the `!` command writes, pumping the screen so the app keeps
/// draining the pty. The file's existence is the synchronization; the deadline only bounds a
/// failure.
#[cfg(target_os = "macos")]
fn wait_for_bang_file(pty: &mut Pty, path: &Path) -> bool {
    let deadline = Instant::now() + wait_positive();
    while Instant::now() < deadline {
        pty.pump();
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}

/// The system launcher: ADR 0016 acceptance 2 wraps the bin in a deny-default
/// profile.
#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// A capped shell result keeps the truncation marker visible. Runs under `--profile
/// workspace`: since slice 1 of `sandbox-auto-allow-shell` the sandboxed shell runs
/// with no prompt, so the turn goes straight to the marker.
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
            "--profile",
            "workspace",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // No approval: the sandboxed shell runs at once.
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("SHELL-DONE", wait_positive()),
        "the shell command did not finish:\n{}",
        pty.screen()
    );
    let screen = pty.screen();
    assert!(
        screen.contains("bytes discarded"),
        "the truncation marker must be visible:\n{screen}"
    );
    assert!(
        !screen.contains("approve tool call?"),
        "the sandboxed shell must not ask:\n{screen}"
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Slice 1 of `sandbox-auto-allow-shell`: a compound runs without a prompt inside the
/// sandbox — the tier's blanket `allow` covers every segment, so the allow rule (which
/// never matched compounds) is irrelevant to it. The ask itself stays covered end to
/// end by the `pty_approval_*` tests; the unit tests in `workspace_sandbox.rs` pin the
/// unsandboxed compound ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The test body blocks on the pty on purpose; the lock serializes the pty tests.
#[allow(clippy::await_holding_lock)]
async fn pty_a_sandboxed_compound_shell_command_runs_without_asking() {
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
            "--profile",
            "workspace",
            "--allow-shell-command",
            &allowed,
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The simple command is allowed by the rule: it runs with no key pressed, and its output
    // proves it actually ran.
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("ALLOWED-OUTPUT-42", wait_positive()),
        "the allowed command did not run:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("FIRST-DONE", wait_positive()),
        "the allowed command was blocked:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "an allowed simple command must not ask:\n{}",
        pty.screen()
    );

    // The compound runs at once under the tier blanket: no approval box, the turn goes
    // straight to the marker.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("SECOND-DONE", wait_positive()),
        "the compound command did not finish:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("approve tool call?"),
        "a sandboxed compound must not ask:\n{}",
        pty.screen()
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    // The tier blanket allowed the compound: the decision is durable and audited as policy.
    let session = std::fs::read_to_string(sessions.join("shell-compound.jsonl")).unwrap();
    let compound_decision = session
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["event"] == "PermissionResolved")
        .map(|value| value["data"].clone())
        .find(|data| data["call_id"] == "call_2")
        .expect("the compound call must have a decision");
    assert_eq!(
        compound_decision["decision"], "allow",
        "{compound_decision:#?}"
    );
    assert_eq!(
        compound_decision["approver"], "policy",
        "the tier blanket decided it: {compound_decision:#?}"
    );
}

/// A panic inside the driver task is caught by the runtime. The UI must notice that the
/// session is gone: leave `Running`, name the reason in the status bar, and refuse `/fork`
/// (ADR 0022 §1's idle session no longer exists). It stays so the reason is readable; the
/// user quits with the ordinary keys, which restore the terminal.
#[test]
fn pty_a_driver_panic_ends_the_session_and_refuses_fork() {
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
    // The session ends in place: the notice names the reason and the app stays.
    assert!(
        pty.wait_for_screen("the session ended: driver panicked", wait_positive()),
        "the session-ended notice is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("driver task panic test", wait_positive()),
        "the reason is the panic message:\n{}",
        pty.screen()
    );
    // `/fork` is refused: the session is over.
    pty.send(b"/fork\r");
    assert!(
        pty.wait_for_screen(
            "the session has ended; there is nothing to fork",
            wait_positive()
        ),
        "`/fork` was not refused:\n{}",
        pty.screen()
    );
    // The user quits with the ordinary keys; the terminal is restored. The exit is still 1:
    // the driver's failure is reported on the normal screen by `main` after the loop.
    pty.quit();
    assert_eq!(
        pty.app_exit_code(),
        Some(1),
        "the driver failure is still reported"
    );
    let raw_text = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(raw_text.contains("\u{1b}[?1049l"), "left alternate screen");
    assert!(raw_text.contains("\u{1b}[?25h"), "cursor restored");
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
    assert!(pty.wait_for_screen("message", wait_positive()));

    pty.send(b"\x1b[200~paste line one\npaste line two\npaste line three\x1b[201~");
    assert!(
        pty.wait_for_screen("paste line one", wait_positive()),
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

/// An IME commits a whole word in one burst; every character must reach the input in one
/// frame, not one poll period apart (11 CJK characters appeared ~50 ms
/// apart, because `read_input` waited the full `INPUT_POLL` before draining crossterm's
/// buffer).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_committed_ime_string_reaches_the_input_in_one_frame() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let dir = TempDir::new("ime");
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
    assert!(pty.wait_for_screen("message", wait_positive()));
    // A mixed CJK/ASCII run written as one burst (what an IME commit produces).
    let text = "你好世界abc测试中文输入法";
    let start = std::time::Instant::now();
    pty.send(text.as_bytes());
    assert!(
        pty.wait_for_screen(text, wait_positive()),
        "the whole string must reach the input:\n{}",
        pty.screen()
    );
    let elapsed = start.elapsed();
    // The pre-fix behavior was one 50 ms poll period per buffered character (~550 ms here).
    // 150 ms is well above the one-frame latency (a frame renders in a few ms, and the state
    // wait polls every 20 ms) and far below the pre-fix time, so it fails on the regression,
    // not on a loaded machine.
    assert!(
        elapsed < std::time::Duration::from_millis(150),
        "the string took {elapsed:?}; the pre-fix behavior was ~50 ms per character"
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
    assert!(pty.wait_for_screen("message", wait_positive()));

    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("first answer", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("second answer", wait_positive()),
        "{}",
        pty.screen()
    );

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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hi\r");
    assert!(
        pty.wait_for_screen("HEADER-OK", wait_positive()),
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
    assert!(
        first.wait_for_screen("message", wait_positive()),
        "{}",
        first.screen()
    );
    first.send(b"one\r");
    assert!(
        first.wait_for_screen("FIRST", wait_positive()),
        "{}",
        first.screen()
    );
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
    assert!(
        second.wait_for_screen("message", wait_positive()),
        "{}",
        second.screen()
    );
    second.send(b"two\r");
    assert!(
        second.wait_for_screen("SECOND", wait_positive()),
        "{}",
        second.screen()
    );
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
    assert_eq!(pty.app_exit_code(), Some(2));
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The session is open, so the status bar reports what connected. The fixture exposes 17
    // tools.
    assert!(
        pty.wait_for_screen("mcp 1/17", wait_positive()),
        "the status bar does not show the MCP counts:\n{}",
        pty.screen()
    );

    pty.send(b"call the echo tool\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
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
        pty.wait_for_screen("MCP-DONE", wait_positive()),
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

/// ADR 0027: `tui.brand` is the one source for the footer's `brand` item and the
/// startup banner's one-line fallback. At a width below the wordmark the fallback shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_brand_config_is_the_banner_fallback() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n[tui]\nbrand = \"ʀ\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 20);
    assert!(
        pty.wait_for_screen("ʀ radian", wait_positive()),
        "the configured brand is not in the fallback banner:\n{}",
        pty.screen()
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
    skip_inside_sandbox!("pty_the_workspace_write_notice_names_the_mcp_sandbox_gap");
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
            // This notice belongs to the `workspace` tier's facts (ADR 0043: not the default).
            "--profile",
            "workspace",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The sandbox facts line is one of the folded startup lines.
    pty.expand_startup();
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

/// ADR 0027: the startup summary's sandbox word is the **real** sandbox, never `none`. The
/// bug was searching the `workspace` tier's description text, which does not name the backend,
/// so macOS showed `workspace · none`. the summary says the plain words
/// ("macOS sandbox" / "Linux sandbox") and the expanded `profile:` line keeps the exact
/// backend name.
///
/// ADR 0016 acceptance 4 rides along: the process is started with the environment
/// variables someone might expect to select the `inherited` backend (`RADIAN_SANDBOXED`,
/// `RADIAN_SANDBOX_INHERIT`). Detection may only ask the kernel, so outside a sandbox the real
/// backend is still the one probed and named. Mutation: reading an environment variable in
/// `confined_by_outer_sandbox` (the summary would read `inherited`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_startup_summary_names_the_real_sandbox_backend() {
    skip_inside_sandbox!("pty_the_startup_summary_names_the_real_sandbox_backend");
    let _serial = pty_guard();
    let dir = TempDir::new("summary-sandbox");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "summary-sandbox",
            "--workspace",
            ws.to_str().unwrap(),
            // ADR 0043: the enforced tier is named by the flag now.
            "--profile",
            "workspace",
        ],
        &[("RADIAN_SANDBOXED", "1"), ("RADIAN_SANDBOX_INHERIT", "1")],
        30,
        120,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    let screen = pty.screen();
    // The collapsed summary line carries the tier, the sandbox word and the rest.
    let summary = screen
        .lines()
        .find(|line| line.contains("context ") && line.contains("ask_user"))
        .unwrap_or_else(|| panic!("no summary line:\n{screen}"));
    assert!(
        !summary.contains("· none ·"),
        "the workspace tier has a sandbox; it must not read as none: {summary:?}"
    );
    let expected = if cfg!(target_os = "macos") {
        "macOS sandbox"
    } else {
        "Linux sandbox"
    };
    assert!(
        summary.contains(expected),
        "the summary must name the sandbox in plain words ({expected}): {summary:?}"
    );
    // The exact backend name stays in the expanded `profile:` line.
    pty.expand_startup();
    let expanded = pty.screen();
    let backend = if cfg!(target_os = "macos") {
        "macos-seatbelt"
    } else {
        "linux-bwrap"
    };
    assert!(
        expanded.contains(backend),
        "the expanded profile line keeps the real backend name ({backend}): {expanded}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0043 decision 2: with no flags the session starts `full` and the startup summary says
/// `no sandbox`. Putting the default back to `workspace` is the mutation this catches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_default_tier_is_full_with_no_sandbox() {
    skip_inside_sandbox!("pty_the_default_tier_is_full_with_no_sandbox");
    let _serial = pty_guard();
    let dir = TempDir::new("default-full");
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
            "default-full",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        30,
        120,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    let screen = pty.screen();
    let summary = screen
        .lines()
        .find(|line| line.contains("context ") && line.contains("ask_user"))
        .unwrap_or_else(|| panic!("no summary line:\n{screen}"));
    assert!(
        summary.contains("full"),
        "the default tier is `full`: {summary:?}"
    );
    assert!(
        summary.contains("no sandbox"),
        "the default must say there is no sandbox: {summary:?}"
    );
    assert!(
        summary.contains("no prompts"),
        "the default must say nothing prompts: {summary:?}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0043 decision 2: the project declaration `<workspace>/.radian/config.toml` can only
/// narrow. `--trust` skips the startup modal (a project config is a project resource), and the
/// summary then shows `workspace`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_project_config_narrows_the_tier() {
    skip_inside_sandbox!("pty_a_project_config_narrows_the_tier");
    let _serial = pty_guard();
    let dir = TempDir::new("project-workspace");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(ws.join(".radian")).unwrap();
    std::fs::write(
        ws.join(".radian/config.toml"),
        "[permissions]\nprofile = \"workspace\"\n",
    )
    .unwrap();
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
            "project-workspace",
            "--workspace",
            ws.to_str().unwrap(),
            "--trust",
        ],
        30,
        120,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    let screen = pty.screen();
    let summary = screen
        .lines()
        .find(|line| line.contains("context ") && line.contains("ask_user"))
        .unwrap_or_else(|| panic!("no summary line:\n{screen}"));
    assert!(
        summary.contains("workspace"),
        "the project must narrow the default to `workspace`: {summary:?}"
    );
    assert!(
        summary.contains("sandbox"),
        "the summary must show the sandbox: {summary:?}"
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.expand_startup();
    // Wait for the line itself: expand_startup returns on the first expanded frame it sees,
    // and under load the pty can hold only part of that frame (gate run, load ~300).
    assert!(
        pty.wait_for_screen(
            "auto: tool calls are approved without asking",
            wait_positive()
        ),
        "the startup must say approvals are off:\n{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("sum=42", wait_positive()),
        "the command output is missing, so the shell did not run:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("YES-DONE", wait_positive()),
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
        tool_reply("call_1", "read", &json!({"path": "."}).to_string()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send("看看 @notes.txt\r".as_bytes());
    assert!(
        pty.wait_for_screen("MENTION-OK", wait_positive()),
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
        pty.wait_for_screen("[file: notes.txt,", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"see @blob.bin\r");
    assert!(
        pty.wait_for_screen("not sent:", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send("看 @no".as_bytes());
    pty.send(b"\t");
    assert!(
        pty.wait_for_screen("@notes.txt", wait_positive()),
        "Tab did not complete the reference:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("TAB-OK", wait_positive()),
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
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit on a bad workspace"
    );
    assert_eq!(pty.app_exit_code(), Some(2));
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
            // The deny-read boundary is the `workspace` tier's (ADR 0020/0043).
            "--profile",
            "workspace",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit on a nested workspace"
    );
    assert_eq!(pty.app_exit_code(), Some(2));
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
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the refusal did not exit"
    );
    assert_eq!(pty.app_exit_code(), Some(2));
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
    assert!(
        retry.wait_for_exit(wait_positive()).is_some(),
        "the retry did not exit"
    );
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
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the refusal did not exit"
    );
    assert_eq!(pty.app_exit_code(), Some(2));
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit"
    );
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

/// ADR 0013 T4 (audit 2026-10-15): the panic log's redaction list must carry **every** key in
/// `auth.json`, not only the startup provider's. A second logged-in provider's key appearing in
/// runtime text (here a synthesized panic) must not reach the log or the terminal.
#[test]
fn pty_the_panic_log_redacts_every_auth_key_not_only_the_startup_one() {
    let _serial = pty_guard();
    let dir = TempDir::new("panic-all-keys");
    let sessions = dir.path().join("sessions");
    let home = dir.path().to_str().unwrap();
    let startup = "SECRET-STARTUP-KEY-aaa";
    let other = "SECRET-OTHER-PROVIDER-KEY-bbb";
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "panic-all-keys",
            "--panic-message",
            &format!("boom {other}"),
        ],
        // Two entries: the startup provider (`openai-compatible`) and `anthropic`, whose key
        // is never used by this run but is still a secret that must not be written out.
        &[
            ("OPENAI_API_KEY", startup),
            ("ANTHROPIC_API_KEY", other),
            ("HOME", home),
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit"
    );
    assert_eq!(pty.app_exit_code(), Some(101), "a panic exits 101");

    let diagnostics = diagnostics_dir(dir.path());
    let log_path = std::fs::read_dir(&diagnostics)
        .unwrap_or_else(|e| panic!("no diagnostics directory {}: {e}", diagnostics.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.to_string_lossy().ends_with(".panic.log"))
        .unwrap_or_else(|| panic!("no panic log in {}", diagnostics.display()));
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("no panic log at {}: {e}", log_path.display()));
    assert!(
        !log.contains(other),
        "every auth.json key must be redacted, not only the startup provider's: {log}"
    );
    assert!(
        log.contains("<redacted>"),
        "redaction marker missing: {log}"
    );

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        !raw.contains(other),
        "the terminal output must not carry the other provider's key: {raw}"
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
        assert!(
            pty.wait_for_screen("message", wait_positive()),
            "{}",
            pty.screen()
        );
        let pid = app_pid(&pty);
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(&pid)
            .status()
            .unwrap();
        assert!(status.success(), "kill -{signal} {pid}");
        assert!(
            pty.wait_for_exit(wait_positive()).is_some(),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("ANTHROPIC-TUI-REPLY", wait_positive()),
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
        first.wait_for_exit(wait_positive()).is_some(),
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
        second.wait_for_exit(wait_positive()).is_some(),
        "the second run did not exit"
    );
    assert_eq!(second.app_exit_code(), Some(2));
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
            pty.wait_for_exit(wait_positive()).is_some(),
            "{label}: the refusal did not exit"
        );
        assert_eq!(pty.app_exit_code(), Some(2), "{label}");
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
        // Non-transient, so one failure ends the turn (ADR 0017 retries only
        // connection failures, dropped streams, idle timeouts and 408/429/5xx/529).
        Reply::status(400, "provider is down"),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("turn failed", wait_positive()),
        "the failure is not shown:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("HTTP 400", wait_positive()),
        "the HTTP status is not shown:\n{}",
        pty.screen()
    );
    // Still usable: a second line opens a new turn that succeeds.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("SECOND-OK", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("model context overflow", wait_positive()),
        "the overflow is not shown:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("compaction could not keep the request", wait_positive()),
        "the hint is not shown:\n{}",
        pty.screen()
    );
    // Still usable: a second line opens a new turn that succeeds.
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("SECOND-OK", wait_positive()),
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
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("B 15000 from --context-window", wait_positive()),
        "the startup notice does not name the explicit window and its B:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// ADR 0015 (a zero window is unset): `[model] context_window = 0` leaves the window unknown in
/// the TUI too, exactly like the key being absent — the startup notice says there is no budget,
/// and no number is invented. The reader maps the key's zero to `None` before either host sees
/// it, so only a reader that refused `0` would fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_zero_context_window_in_the_config_leaves_the_budget_off() {
    let _serial = pty_guard();
    let dir = TempDir::new("zero-window");
    let home = TempDir::new("zero-window-home");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "zero-window",
            "--profile",
            "full",
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        "schema_version = 1\n[model]\ncontext_window = 0\n",
        24,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.expand_startup();
    assert!(
        pty.wait_until(wait_positive(), |screen| {
            let notice = screen.split_whitespace().collect::<Vec<_>>().join(" ");
            notice.contains("context window: window unknown") && notice.contains("no budget")
        }),
        "a zero `[model] context_window` must be unset: the startup notice must say the window \
         is unknown:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0015: the flag's zero is a **usage error** in the TUI too, exactly as under `radian exec`
/// — the user typed a window, so "not given" is not what they meant. The refusal happens before
/// the terminal is entered and before a session file exists; a host that read the flag's zero as
/// "unset" would start the app instead (the openai-compatible default provider needs no key for
/// a loopback endpoint, so nothing else can refuse this run).
#[test]
fn pty_a_zero_context_window_flag_is_a_usage_error() {
    let _serial = pty_guard();
    let dir = TempDir::new("zero-window-flag");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            session_dir.to_str().unwrap(),
            "--session-id",
            "zero-window-flag",
            "--context-window",
            "0",
        ],
        20,
        100,
    );
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not refuse `--context-window 0`"
    );
    assert_eq!(pty.app_exit_code(), Some(2));
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    assert!(
        raw.contains("--context-window must be at least 1 token"),
        "{raw}"
    );
    assert!(
        !raw.contains("\u{1b}[?1049h"),
        "the refusal must come before the terminal: {raw}"
    );
    assert_session_dir_empty(&session_dir);
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The memory notice is one of the folded startup lines. Wait for the
    // text itself: `expand_startup` returns on the first expanded frame it sees, and under load
    // the pty can hold only part of that frame. The notice wraps across lines, so compare the
    // screen with its whitespace normalized; the bound only fails a missing notice.
    pty.expand_startup();
    assert!(
        pty.wait_until(wait_positive(), |screen| {
            let notice = screen.split_whitespace().collect::<Vec<_>>().join(" ");
            notice.contains("[memory]")
                && notice.contains("no project linked")
                && notice.contains("injection on, up to 2000 tokens")
        }),
        "the memory startup notice is missing:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0021: `/memory add` in a git workspace registers the project
/// itself, and the TUI shows the one notice line naming it. It must reach the transcript (a
/// `Notice` entry), not stderr, which the fullscreen UI draws over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_memory_add_shows_the_created_project_notice() {
    let _serial = pty_guard();
    let dir = TempDir::new("memory-auto-create");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // A `.git` directory is enough: the identity key comes from the repository, so the write
    // registers the project. No `git` process, no real repository content.
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "mem-auto-create",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.to_str().unwrap())],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(b"/memory add \"hello world\"\r");
    assert!(
        pty.wait_for_screen("created project", wait_positive()),
        "the notice names the new project: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("identity key", wait_positive()),
        "the notice names the identity key: {}",
        pty.screen()
    );
    pty.send(b"/memory list\r");
    assert!(
        pty.wait_for_screen("hello world", wait_positive()),
        "the record is in the new project: {}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "a /memory command must not reach the model"
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(b"/memory project new\r");
    assert!(
        pty.wait_for_screen("now owns", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/memory add \"hello world\"\r");
    assert!(
        pty.wait_for_screen("added", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/memory list\r");
    assert!(
        pty.wait_for_screen("hello world", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "a /memory command must not reach the model"
    );

    // A normal message still works, and the memory is in its system prompt.
    pty.send(b"what do you remember?\r");
    assert!(
        pty.wait_for_screen("REPLY-AFTER-MEMORY", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/memory project new\r");
    assert!(
        pty.wait_for_screen("now owns", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/memory add --global \"remembered\"\r");
    assert!(
        pty.wait_for_screen("added", wait_positive()),
        "{}",
        pty.screen()
    );

    // Start a turn whose reply never completes; the status bar shows `running`.
    pty.send(b"start a long turn\r");
    assert!(
        pty.wait_for_screen("running", wait_positive()),
        "{}",
        pty.screen()
    );
    // `running` is shown at `TurnStarted`, before the request reaches the server; wait for
    // the request itself (polling, like `wait_for_screen`) instead of asserting at once.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.requests().is_empty() && std::time::Instant::now() < deadline {
        pty.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests().len(), 1);

    pty.send(b"/memory list --global\r");
    assert!(
        pty.wait_for_screen("remembered", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/compact\r");
    assert!(
        pty.wait_for_screen("nothing to compact", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // "helo", move left twice (past `o` and `l`) and insert an `l`.
    pty.send(b"helo");
    pty.send(b"\x1b[D\x1b[D");
    pty.send(b"l");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("EDITED-OK", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // `one` Ctrl-J `two`, then Up to the end of the first line and append `!`.
    pty.send(b"one");
    pty.send(&[0x0a]); // Ctrl-J
    pty.send(b"two");
    pty.send(b"\x1b[A"); // Up: the end of `one`, keeping the column
    pty.send(b"!");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("MULTI-OK", wait_positive()),
        "{}",
        pty.screen()
    );
    assert_eq!(last_user_text(&server, 0), "one!\ntwo");

    // Paste inserts at the cursor, not at the end.
    pty.send(b"ac");
    pty.send(b"\x1b[D"); // Left
    pty.send(b"\x1b[200~b\x1b[201~");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("PASTE-OK", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"unsent draft");
    // Wait for the whole line to render before signalling: the input thread delivers bytes
    // and the app redraws them, and a signal must not race the last characters.
    assert!(
        pty.wait_for_screen("unsent draft", wait_positive()),
        "the draft is not in the input box:\n{}",
        pty.screen()
    );
    // Exit by signal, not Ctrl-C: an idle Ctrl-C clears a non-empty input first (owner
    // 2026-10-14), which would discard the draft. A signal exit takes the same exit path
    // (the draft is written, the resume line printed) without clearing the input.
    let pid = app_pid(&pty);
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(&pid)
        .status()
        .unwrap();
    assert!(status.success(), "kill -TERM {pid}");
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit"
    );
    assert_eq!(pty.app_exit_code(), Some(143), "128 + SIGTERM");
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
        pty.wait_for_screen("unsent draft", wait_positive()),
        "the draft is not in the input box:\n{}",
        pty.screen()
    );
    assert!(!draft.exists(), "a resume consumes the draft");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Appends `count` committed filler replies to the session `id`, the last one `{id}-END` (the
/// tail marker a test can look for). A transcript taller than the pty matters twice: a scroll
/// key has somewhere to go (the attach view's footer only leaves `follow` when it can move),
/// and a session that is not attached to is never on screen. Returns the writer, still holding
/// the file's lock, so the caller can append again while a reader watches.
async fn fill_session(dir: &Path, id: &str, count: usize) -> radian_session::SessionJournal {
    use agent_core::TranscriptJournal;
    let path = dir.join(format!("{id}.jsonl"));
    let journal = radian_session::SessionJournal::resume(&path, id).expect("resume to append");
    for index in 0..count {
        let text = if index + 1 == count {
            format!("{id}-END")
        } else {
            format!("{id}-FILLER-{index}")
        };
        journal
            .append(agent_core::JournalRecord::Message {
                index: index + 2,
                message: agent_core::Message::assistant(text),
            })
            .await
            .unwrap();
    }
    journal
}

/// Waits until `path`'s contents contain `needle`. The file is the synchronization (the app
/// writes it from its own task); the deadline only bounds a failure.
fn wait_for_file(path: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
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
        pty.wait_for_screen("alpha", wait_positive()),
        "the picker did not list alpha:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("beta", wait_positive()),
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
        pty.wait_until(wait_positive(), |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("resumed session beta", wait_positive()),
        "Enter did not resume the filtered row:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("BETA-HISTORY", wait_positive()),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    // The next request carries the resumed history, not the other session's.
    pty.send(b"continue\r");
    assert!(
        pty.wait_for_screen("PICKED-REPLY", wait_positive()),
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
    assert!(
        pty.wait_for_screen("alpha", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(&[0x1b]); // Esc
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "Esc did not quit"
    );
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
    assert!(
        pty.wait_for_screen("alpha", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"n");
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("new session", wait_positive()),
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
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("no sessions for this workspace yet", wait_positive()),
        "the no-session notice is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("new session", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"session body text\r");
    assert!(
        pty.wait_for_screen("SESSION-REPLY", wait_positive()),
        "{}",
        pty.screen()
    );

    // `/sessions` names the current session and the way to reopen it.
    pty.send(b"/sessions\r");
    assert!(
        pty.wait_for_screen("--resume cmd", wait_positive()),
        "the session list is missing the resume hint:\n{}",
        pty.screen()
    );

    // `/export` writes the default `<workspace>/radian-cmd.md`.
    pty.send(b"/export\r");
    assert!(
        pty.wait_for_screen("exported to", wait_positive()),
        "no export notice:\n{}",
        pty.screen()
    );
    let export = ws.join("radian-cmd.md");
    assert!(
        wait_for_file(&export, "session body text", wait_positive()),
        "the export file was not written with the session text"
    );

    // Without `--force` it refuses; with `--force` it overwrites.
    pty.send(b"/export\r");
    assert!(
        // The notice wraps, so match a fragment that stays on one line.
        pty.wait_for_screen("ready exists", wait_positive()),
        "the overwrite refusal is missing:\n{}",
        pty.screen()
    );
    std::fs::write(&export, "OLD").unwrap();
    pty.send(b"/export md radian-cmd.md --force\r");
    assert!(
        wait_for_file(&export, "session body text", wait_positive()),
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
        headers: Vec::new(),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"start\r");
    // The first paced chunk has arrived, so the turn is running.
    assert!(
        pty.wait_for_screen("TURN-ONE-", wait_positive()),
        "the turn did not start:\n{}",
        pty.screen()
    );
    // Open the picker while the turn runs: it opens unfocused, so the composer keeps the keys.
    pty.send(b"/sessions\r");
    assert!(
        pty.wait_for_screen("filter:", wait_positive()),
        "the picker did not open during the turn:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]); // Esc closes the overlay
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("filter:")),
        "Esc did not close the picker:\n{}",
        pty.screen()
    );
    // The turn was not disturbed: its later chunk commits normally.
    assert!(
        pty.wait_for_screen("TURN-TWO", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hi");
    pty.send(b"\r"); // Enter: a newline, not a send
    pty.send(&[0x13]); // Ctrl-S: the rebound send
    assert!(
        pty.wait_for_screen("SENT", wait_positive()),
        "{}",
        pty.screen()
    );
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
        pty.wait_for_screen("❯ hello parent config", wait_positive()),
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
    assert!(
        parent.wait_for_screen("message", wait_positive()),
        "{}",
        parent.screen()
    );
    parent.send(b"hello parent\r");
    assert!(
        parent.wait_for_screen("PARENT-DONE", wait_positive()),
        "{}",
        parent.screen()
    );

    parent.send(b"/fork\r");
    assert!(
        parent.wait_for_screen("forked at seq", wait_positive()),
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
        parent.wait_for_screen(&format!("--resume {child_id}"), wait_positive()),
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
        child.wait_for_screen("hello parent", wait_positive()),
        "the child history is not rendered:\n{}",
        child.screen()
    );
    child.send(b"continue\r");
    assert!(
        child.wait_for_screen("CHILD-DONE", wait_positive()),
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

/// ADR 0022 §1: a `/fork` requested while the turn's boundary is still being written waits for
/// the journaled `TurnFinished` (a controllable gap via `RADIAN_TURN_COMMIT_DELAY_MS`) instead
/// of copying an empty history. The classic footer shows the turn state, so the test can wait
/// for `idle` (the `RunFinished`) while the commit is still pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_fork_waits_for_the_turn_to_be_committed() {
    let _serial = pty_guard();
    let dir = TempDir::new("fork-commit-wait");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("PARENT-DONE")]).await;
    let config = "schema_version = 1\n[tui.footer]\npreset = \"classic\"\n";
    let mut pty = Pty::spawn_with_config(
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
        &[("RADIAN_TURN_COMMIT_DELAY_MS", "1500")],
        config,
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello parent\r");
    assert!(
        pty.wait_for_screen("PARENT-DONE", wait_positive()),
        "{}",
        pty.screen()
    );
    // The turn is idle (the agent's `RunFinished`), but the journaled `TurnFinished` is still
    // ~1.5 s away.
    assert!(
        pty.wait_for_screen("idle", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/fork\r");
    assert!(
        pty.wait_for_screen("forked at seq", wait_positive()),
        "the fork never ran after the commit:\n{}",
        pty.screen()
    );
    // The child has the parent's whole turn, not an empty history.
    let child = std::fs::read_dir(&sessions)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl") && !path.ends_with("parent.jsonl")
        })
        .expect("a child session file");
    let text = std::fs::read_to_string(&child).unwrap();
    assert!(
        text.contains("hello parent"),
        "the child history is empty:\n{text}"
    );
    assert!(text.contains("PARENT-DONE"), "{text}");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0022 §1: when the commit takes longer than `FORK_COMMIT_WAIT`, the `/fork` gives up with
/// a notice and creates no child session (never an empty one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_fork_gives_up_when_the_turn_is_still_being_written() {
    let _serial = pty_guard();
    let dir = TempDir::new("fork-commit-timeout");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("PARENT-DONE")]).await;
    let config = "schema_version = 1\n[tui.footer]\npreset = \"classic\"\n";
    let mut pty = Pty::spawn_with_config(
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
        &[("RADIAN_TURN_COMMIT_DELAY_MS", "8000")],
        config,
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello parent\r");
    assert!(
        pty.wait_for_screen("PARENT-DONE", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("idle", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/fork\r");
    assert!(
        pty.wait_for_screen("still writing the turn; try again", wait_positive()),
        "the fork did not time out:\n{}",
        pty.screen()
    );
    // No child session file was created. The session's own `<id>.jobs/` directory is not a
    // session (ADR 0039 decision 1), so only `.jsonl` files count here.
    let mut files: Vec<String> = std::fs::read_dir(&sessions)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    files.sort();
    assert_eq!(
        files,
        vec!["parent.jsonl".to_string()],
        "no empty child: {files:?}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("sleep 5", wait_positive()),
        "the sleeping tool call did not start:\n{}",
        pty.screen()
    );

    pty.send(b"/fork\r");
    assert!(
        pty.wait_for_screen("finish the current turn before forking", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"m1\r");
    assert!(
        pty.wait_for_screen("ALPHA-MARKER", wait_positive()),
        "{}",
        pty.screen()
    );
    for i in 2..=8 {
        pty.send(format!("m{i}\r").as_bytes());
        assert!(
            pty.wait_for_screen(&format!("filler-{i}-line-11"), wait_positive()),
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
        pty.wait_for_screen("match 1/1", wait_positive()),
        "the find status is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("ALPHA-MARKER", wait_positive()),
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
    // The exit path's timestamps (`--exit-trace`): on a failure this says which step was
    // slow (or that the POLLHUP path never fired).
    let trace = dir.path().join("exit-trace");
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
            "--exit-trace",
            trace.to_str().unwrap(),
        ],
        home.path(),
    );

    // The first frame is drawn (its marker exists); from now on the app writes nothing.
    let mut drained = Vec::new();
    let mut buf = [0u8; 65536];
    // These two waits poll with a short sleep. `frozen` is a file the app writes from its own
    // task: without a filesystem watcher (kqueue/FSEvents, i.e. a dependency) there is no
    // notification to block on. The exit wait could use `waitpid` on a helper thread with a
    // channel timeout, but the child handle is shared with `KillOnDrop` (a double wait) and the
    // outcome of the investigation is that the app does not exit at all on a crossterm reader
    // hang, so the deadline is what the test is about — not the wait mechanism.
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
    let closed_at = Instant::now();
    drop(master);

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let steps = std::fs::read_to_string(&trace).unwrap_or_default();
            panic!(
                "the app did not exit within 5 s of its terminal closing: the input POLLHUP \
                 path did not fire, and drawing is frozen (--freeze-draw) so the draw path \
                 cannot rescue it either. Exit trace (review: a trace whose last point is \
                 `poll-idle` means the input thread is inside crossterm's reader and its read \
                 is looping on EOF):\n{steps}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(0),
        "the terminal-gone exit is the ordinary clean one (exit 0, no signal): {status:?}"
    );
    if std::env::var_os("RADIAN_FLAKY_VERBOSE").is_some() {
        // Investigation aid: how long the exit path took and where the time went.
        eprintln!(
            "closed-master: exit took {:?}\n{}",
            closed_at.elapsed(),
            std::fs::read_to_string(&trace).unwrap_or_default()
        );
    }
}

/// The exit line must not hang the process when the terminal stops draining (a full pty output
/// buffer, or XOFF). `main` prints it **after** `runtime.shutdown_timeout`, so the signal task
/// — the watchdog that is the only other way a stuck process ends — is already gone; an
/// unbounded `writeln!` to a full pty would then block forever (small-fixes-3).
///
/// `--exit-line-filler` makes the clean-exit reason line larger than the pty output buffer, so
/// the write is guaranteed to block once the test stops draining. `--freeze-draw` parks the app
/// after the first frame, so the only writes left on the exit path are the terminal restore
/// (small, and the buffer has room for it after the drain) and the exit line itself. The child
/// must still exit within the bound; the deadline is a failure bound, not synchronization.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_full_terminal_cannot_hang_the_exit_line() {
    let _serial = pty_guard();
    let dir = TempDir::new("exit-line-hang");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.path().join("sessions");
    let frozen = dir.path().join("frozen");
    let home = TempDir::new("exit-line-hang-home");
    let (mut child, mut master) = spawn_direct(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "exit-line-hang",
            "--offline-catalog",
            "--workspace",
            ws.to_str().unwrap(),
            // The app stops drawing once the first frame is out; with no further writes the pty
            // buffer cannot fill from the draw path.
            "--freeze-draw",
            frozen.to_str().unwrap(),
            // Larger than any pty output buffer, so the exit line's write cannot complete once
            // the test stops reading.
            "--exit-line-filler",
            "1048576",
        ],
        home.path(),
    );

    // The first frame is larger than the pty buffer, so the app blocks in `draw` until someone
    // reads; the marker appears only once the frame is fully written.
    let mut drained = Vec::new();
    let mut buf = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(20);
    while !frozen.exists() {
        if let Ok(n) = master.read(&mut buf) {
            drained.extend_from_slice(&buf[..n]);
        }
        assert!(
            Instant::now() < deadline,
            "the app never drew its first frame; output: {}",
            String::from_utf8_lossy(&drained)
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the app exited before the signal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Empty the pty buffer: the app is frozen, so nothing else is coming from the draw path, and
    // the restore sequences need room. A non-blocking master reports "nothing buffered" as an
    // error, which ends the drain.
    loop {
        match master.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained.extend_from_slice(&buf[..n]),
        }
    }

    // Stop draining and ask the app to leave. Its exit path writes the restore sequences (they
    // fit) and then the oversized exit line, whose write blocks on the full buffer.
    // SAFETY: signal delivery to our own child process.
    unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM) };

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the app did not exit within 10 s of SIGTERM: the exit line blocked on the full pty \
             (it is printed after runtime.shutdown_timeout, when the signal watchdog is gone)"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(128 + 15), "128 + SIGTERM: {status:?}");
}

/// The remaining window: the main loop can be stuck inside a long frame (a terminal write that
/// blocks because nobody drains it), so it never sees the signal flag and only the signal task's
/// watchdog can end the process. `--slow-draw` parks every frame, which makes that hang
/// deterministic; `--block-input` still writes its marker, so the test signals only once the app
/// is up. (Letting the **first frame** overflow the pty output buffer parks the loop too — that
/// is what [`spawn_direct`] documents — but only until the frame's size crosses the buffer's,
/// so the `bar` input's shorter frame is no longer a reliable hang and the hook is used instead.)
/// A SIGHUP must still end the process, forced by the signal task's watchdog with the shell
/// convention 128+SIGHUP.
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
            // Longer than the test's 10 s deadline: every frame parks the loop, so the loop is
            // inside one when the signal arrives and only the watchdog can end it.
            "--slow-draw",
            "60000",
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
        assert!(
            pty.wait_for_screen("message", wait_positive()),
            "the app did not start"
        );
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

/// [`frame_count`] after the pty output has gone quiet for `QUIET`.
///
/// `frame_count` counts the bytes the test's reader thread has **received**, and the app writes
/// a frame's cursor-show (`[?25h`) *after* the frame text that `wait_for_screen` matches. Under
/// load the reader can lag, so a bare `frame_count` right after `wait_for_screen` can read the
/// startup frames as zero — and then a later read counts them as if the input under test had
/// drawn them (`left: 2, right: 0`). Waiting for the output to stop changing makes the baseline
/// a state, not a race. The wait is bounded; on timeout it returns the current count.
fn settled_frame_count(pty: &mut Pty) -> usize {
    const QUIET: std::time::Duration = std::time::Duration::from_millis(250);
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
    let deadline = std::time::Instant::now() + LIMIT;
    let mut seen = pty.raw().len();
    let mut changed = std::time::Instant::now();
    loop {
        pty.pump();
        let len = pty.raw().len();
        if len != seen {
            seen = len;
            changed = std::time::Instant::now();
        } else if changed.elapsed() >= QUIET || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    frame_count(pty)
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
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    let before = settled_frame_count(&mut pty);
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

/// ADR 0027: a **visible** spinner redraws on its interval (the test above pins
/// the idle case). `arc` turns every 250 ms, so a ~1.2 s window must show several frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_visible_spinner_redraws_on_its_interval() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n[tui]\nspinner = \"arc\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 20, 100);
    assert!(
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    let _ = settled_frame_count(&mut pty);
    // A turn that never finishes keeps the spinner visible.
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("Working", wait_positive()),
        "the input rule must show the running state: {}",
        pty.screen()
    );
    let start = frame_count(&pty);
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let after = frame_count(&pty);
    pty.quit();
    assert!(
        after >= start + 3,
        "the arc spinner must redraw about every 250 ms: {start} -> {after}"
    );
}

/// ADR 0027: with truecolor the default pulse **breathes** — the `⏺` takes the 16
/// colours of one sine period — and the loop wakes at the step (~125 ms), not at revision 10's
/// 500 ms switch. The wire carries each colour as `38;2;R;G;B` in front of the glyph, so the
/// colours themselves are checkable, not just the frame count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_pulse_breathes_through_rgb_colours() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(
        &[],
        &[("OPENAI_API_KEY", "sk-test"), ("COLORTERM", "truecolor")],
        &config,
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    let _ = settled_frame_count(&mut pty);
    // A turn that never finishes keeps the spinner visible.
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("Working", wait_positive()),
        "the input rule must show the running state: {}",
        pty.screen()
    );
    let start = frame_count(&pty);
    std::thread::sleep(std::time::Duration::from_millis(2200));
    let after = frame_count(&pty);
    let breathed = dot_colours(&pty.raw());
    pty.quit();
    assert!(
        after >= start + 10,
        "the breath must redraw about every 125 ms (a two-colour switch would be 4): {start} -> {after}"
    );
    // The breath's 16 samples hold 9 distinct colours (the sine's levels 0, 1, 2, 5, 8, 11, 14, 15,
    // 16), and one sample is missed whenever a wake lands late, so the exact ends need not appear in
    // a given window: a two-colour switch would show 2 colours, a rotating frame 1.
    assert!(
        breathed.len() >= 6,
        "the dot must take the breath's colours: {breathed:?}"
    );
    let theme = radian_tui::theme::Theme::dark();
    let rgb = |style: ratatui::style::Style| match style.fg {
        Some(ratatui::style::Color::Rgb(r, g, b)) => (r, g, b),
        other => panic!("an RGB theme role: {other:?}"),
    };
    // Every colour it took is a point on the dim -> accent line (the breath is one interpolation,
    // not two unrelated colours), and the set reaches both ends of it.
    let (lowest, highest) = breathed
        .iter()
        .map(|colour| {
            (
                colour,
                mix_of(*colour, rgb(theme.truncation), rgb(theme.notice_label)),
            )
        })
        .fold((1.0f64, 0.0f64), |(low, high), (_, t)| {
            (low.min(t), high.max(t))
        });
    assert!(
        lowest <= 0.15,
        "the breath must reach the dim end: {breathed:?}"
    );
    assert!(highest >= 0.85, "and the accent: {breathed:?}");
}

/// Where `colour` sits between the dim end and the accent (0 = dim, 1 = accent), asserting that it
/// is on that line: the channel that moves most fixes the position and every other channel must
/// land within one rounding step of it.
fn mix_of(colour: (u8, u8, u8), dim: (u8, u8, u8), accent: (u8, u8, u8)) -> f64 {
    let colour = [colour.0, colour.1, colour.2];
    let dim = [dim.0, dim.1, dim.2];
    let accent = [accent.0, accent.1, accent.2];
    let spread: [i32; 3] = std::array::from_fn(|i| i32::from(accent[i]) - i32::from(dim[i]));
    let widest = (0..3usize)
        .max_by_key(|i| spread[*i].abs())
        .expect("three channels");
    let t =
        f64::from(i32::from(colour[widest]) - i32::from(dim[widest])) / f64::from(spread[widest]);
    for (i, moved) in spread.iter().enumerate() {
        let expected = f64::from(dim[i]) + t * f64::from(*moved);
        assert!(
            (f64::from(colour[i]) - expected).abs() <= 1.0,
            "{colour:?} is not on the dim -> accent line (channel {i})"
        );
    }
    t
}

/// The distinct 24-bit colours written immediately in front of the pulse glyph (`⏺`), in the
/// order they first appear: the breath as it reached the terminal.
fn dot_colours(raw: &[u8]) -> Vec<(u8, u8, u8)> {
    let text = String::from_utf8_lossy(raw);
    let mut out: Vec<(u8, u8, u8)> = Vec::new();
    let mut at = 0;
    while let Some(found) = text[at..].find("38;2;") {
        let params = at + found + "38;2;".len();
        at = params;
        let Some((rgb, tail)) = text[params..].split_once('m') else {
            break;
        };
        if !tail.starts_with('⏺') {
            continue;
        }
        let mut parts = rgb.split(';');
        let rgb = match (parts.next(), parts.next(), parts.next()) {
            (Some(r), Some(g), Some(b)) => (r, g, b),
            _ => continue,
        };
        if let (Ok(r), Ok(g), Ok(b)) = (
            rgb.0.parse::<u8>(),
            rgb.1.parse::<u8>(),
            rgb.2.parse::<u8>(),
        ) && !out.contains(&(r, g, b))
        {
            out.push((r, g, b));
        }
    }
    out
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
        pty.wait_for_screen("type a message", wait_positive()),
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
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the app did not exit"
    );
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
        pty.wait_for_screen("type a message", wait_positive()),
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
        pty.wait_for_screen("zzz", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"m1\r");
    assert!(
        pty.wait_for_screen("FIRST-MARKER", wait_positive()),
        "{}",
        pty.screen()
    );
    for i in 2..=8 {
        pty.send(format!("m{i}\r").as_bytes());
        assert!(
            pty.wait_for_screen(&format!("filler-{i}-line-11"), wait_positive()),
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
        pty.wait_for_screen("half-written-draft", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(String::from_utf8_lossy(&pty.raw()).contains(MOUSE_ON));

    // Enough notches (3 lines each) to bring the first reply back into view.
    for _ in 0..40 {
        pty.send(&wheel(true));
    }
    assert!(
        pty.wait_for_screen("FIRST-MARKER", wait_positive()),
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
        pty.wait_until(wait_positive(), |screen| !screen.contains("FIRST-MARKER")),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"draft");
    assert!(
        pty.wait_for_screen("draft", wait_positive()),
        "{}",
        pty.screen()
    );
    // A motion report (`ESC[<35;x;yM` = Moved) and a click must change nothing: the draft
    // stays, and the app never enabled any-motion reporting.
    pty.send(b"\x1b[<35;10;10M");
    pty.send(b"\x1b[<0;10;10M");
    // The next key proves the app is alive and the draft survived.
    pty.send(b"-ok");
    assert!(
        pty.wait_for_screen("draft-ok", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
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
        assert!(
            pty.wait_for_screen("message", wait_positive()),
            "{}",
            pty.screen()
        );
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
            pty.wait_for_exit(wait_positive()).is_some(),
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
        assert!(
            pty.wait_for_screen("message", wait_positive()),
            "{}",
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
            pty.wait_for_exit(wait_positive()).is_some(),
            "the app did not exit"
        );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/sessions\r");
    assert!(
        pty.wait_for_screen("filter:", wait_positive()),
        "the picker did not open:\n{}",
        pty.screen()
    );
    // The header's `--resume <id>` hint is drawn with the picker's rows; `filter:` alone can
    // be up a frame earlier under load. Wait for both before snapshotting.
    assert!(
        pty.wait_until(wait_positive(), |screen| screen.contains("filter:")
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
        pty.wait_until(wait_positive(), |screen| {
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
    assert!(
        pty.wait_for_screen("filter:", wait_positive()),
        "{}",
        pty.screen()
    );
    // Filter to beta and resume it.
    pty.send(b"beta");
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    // Wait for the resumed session to open (the picker's phase ends first), then expand the
    // startup block so the resumed-session line is on screen.
    assert!(
        pty.wait_for_screen("BETA-HISTORY", wait_positive()),
        "the resumed history is not on screen:\n{}",
        pty.screen()
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("resumed session beta", wait_positive()),
        "the picker did not resume the selection:\n{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("PICKED", wait_positive()),
        "{}",
        pty.screen()
    );
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

/// (screenshot): `--pick` enters the alternate screen **before** the session
/// exists, so every startup diagnostic printed with `eprintln!` between the picker and the
/// session lands on the alternate screen. The renderer diffs a frame against the *previous*
/// frame's cells and skips the ones that did not change, so those bytes stay where they were
/// written: `[control] <sock>` and the trust status line sat drawn over the input box and never
/// cleared. On this path the startup diagnostics must not reach the terminal at all.
///
/// The check reads the **raw** output, not the `vt100` screen. The raw stream is the direct
/// evidence that the bytes reached the terminal at all; which cell each one ends up in depends on
/// the frame diff and the layout (at 24x100 the dumped screen showed them in the composer row and
/// the footer row, still there after the session's first frame). The second half of the test pins
/// that nothing is lost: the same lines are in the transcript's startup block.
///
/// (p1's earlier screen-content attempts at 24x100 passed on the old code because they waited
/// for the seeded history text — which the picker already shows in the row's summary — so they
/// asserted before the session's first frame. Waiting for `(Ctrl-P to expand)`, which only the
/// session phase draws, is what makes the stray text observable.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_pick_prints_no_startup_diagnostics_into_the_alternate_screen() {
    let _serial = pty_guard();
    let dir = TempDir::new("pick-stderr");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;

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
        pty.wait_for_screen("filter:", wait_positive()),
        "{}",
        pty.screen()
    );
    // Enter resumes the only row (alpha): the picker ends and the session starts in the same
    // terminal, so everything printed in between lands on the alternate screen.
    pty.send(b"\r");
    // Wait for the **session**, not the picker: the row's summary is the seeded user message
    // ("ALPHA-HISTORY"), so waiting for that alone matches the picker and returns too early.
    // The folded startup line exists only once the session phase has drawn.
    assert!(
        pty.wait_for_screen("(Ctrl-P to expand)", wait_positive()),
        "the session did not start after the pick:\n{}",
        pty.screen()
    );
    let raw = String::from_utf8_lossy(&pty.raw()).into_owned();
    let (_, after_enter) = raw
        .split_once("\u{1b}[?1049h")
        .unwrap_or_else(|| panic!("the alternate screen was never entered:\n{raw}"));
    // The two diagnostics this path produces every time: the control socket line and the trust
    // status line (the workspace has no project resources, so the decision is the default).
    assert!(
        !after_enter.contains("[control] "),
        "a startup diagnostic was written into the alternate screen:\n{after_enter}"
    );
    assert!(
        !after_enter.contains("project resources: not loaded"),
        "a startup diagnostic was written into the alternate screen:\n{after_enter}"
    );
    // Deferring must not lose them: the transcript's startup block carries both. The trust line
    // is pushed there for **every** run (ADR 0031 T3), so deferring it as well would show it
    // twice; it must appear exactly once.
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("[control] ", wait_positive()),
        "the control socket line is missing from the startup block:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_until(wait_positive(), |screen| {
            screen.matches("project resources: not loaded").count() == 1
        }),
        "the trust status line must be in the startup block exactly once:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The same rule, for the runtime half of the startup diagnostics: a memory warning (a malformed
/// context file, a bad skill, a failed memory read) is a line the user must see, so it becomes a
/// transcript notice rather than stderr, which the fullscreen UI draws over — ADR 0021 revision
/// 1's rule for the memory *notice*, applied to the warning. On the `--pick` path the startup
/// ones would otherwise land on the alternate screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_memory_warning_is_a_transcript_notice_not_stderr() {
    let _serial = pty_guard();
    let dir = TempDir::new("memory-warning");
    let home = TempDir::new("memory-warning-home");
    // A user skill whose frontmatter declares `allowed-tools`: discovery warns once through the
    // host's memory-warning seam while the session opens.
    std::fs::create_dir_all(home.path().join("skills/warny")).unwrap();
    std::fs::write(
        home.path().join("skills/warny/SKILL.md"),
        "---\nname: warny\ndescription: Warns.\nallowed-tools: Read\n---\nBODY\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "memory-warning",
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        24,
        100,
    );
    // Before the terminal is entered the old line went to stderr, which the alternate screen
    // then covered: the notice is what makes it visible at all. The `allowed-tools`
    // declarations of every skill arrive as one merged line per session.
    assert!(
        pty.wait_for_screen("radian: [memory] 1 skill declares", wait_positive()),
        "the memory warning is not in the transcript:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
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
        pty.wait_for_raw(MOUSE_ON, wait_positive()),
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
        pty.wait_for_screen("filter:", wait_positive()),
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
        pty.wait_for_exit(wait_positive()).is_some(),
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
        60,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // ADR 0024 Q5: this host has an interactive user, so the startup line says so.
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("ask_user: on", wait_positive()),
        "the startup line must say ask_user is on: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("How should it load?", wait_positive()),
        "no question form: {}",
        pty.screen()
    );
    // Wait for the **panel** (its hint), not the transcript block (`[loader]` appears there too).
    assert!(
        pty.wait_for_screen("space select", wait_positive()),
        "the panel must be up: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("loader", wait_positive()),
        "the tab row must show the question header: {}",
        pty.screen()
    );
    // Question 1 (single): Space selects without advancing, `n` adds a note, Tab advances.
    pty.send(b" ");
    pty.send(b"n");
    pty.send(b"note-one");
    pty.send(b"\r"); // save the note
    pty.send(b"\t");
    // Question 2 (multi): Space toggles, Tab advances.
    pty.send(b" ");
    pty.send(b"\t");
    // Question 3 (text): type, Enter -> the Submit tab.
    pty.send(b"radian");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("✓ loader", wait_positive()),
        "the Submit tab lists the answers: {}",
        pty.screen()
    );
    // Enter -> submit.
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("ASK-DONE", wait_positive()),
        "the turn must continue after the answers: {}",
        pty.screen()
    );
    pty.quit();
    // The journal carries the answers and the note (ADR 0024 Q3/R4).
    let journal = std::fs::read_to_string(sessions.join("ask.jsonl")).expect("the journal");
    assert!(
        journal.contains("\"selected\":[\"builtin\"]") && journal.contains("\"note\":\"note-one\""),
        "the answer and the note must be journaled:\n{journal}"
    );
}

/// ADR 0024 R2/R3 end to end: `alt-h` collapses the panel to one line and shows it
/// again without losing the answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_ask_user_alt_h_hides_and_shows() {
    let _serial = pty_guard();
    let dir = TempDir::new("ask-hide");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(ask_reply("HIDE-DONE")).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "ask-hide",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        60,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    // Wait for the **panel** (its hint), not the transcript block (which shows the prompt too).
    assert!(
        pty.wait_for_screen("space select", wait_positive()),
        "{}",
        pty.screen()
    );
    // Answer the first question, then collapse.
    pty.send(b" ");
    assert!(
        pty.wait_for_screen("space select", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"\x1bh"); // alt-h
    assert!(
        pty.wait_for_screen("alt-h to show", wait_positive()),
        "the panel must collapse: {}",
        pty.screen()
    );
    pty.send(b"\x1bh"); // alt-h again
    assert!(
        pty.wait_for_screen("How should it load?", wait_positive()),
        "the panel must come back with the answers: {}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0024 R3 + feedback: `esc` twice declines, and the turn continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_ask_user_esc_declines() {
    let _serial = pty_guard();
    let dir = TempDir::new("ask-decline");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(ask_reply("DECLINE-DONE")).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "ask-decline",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        60,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    // Wait for the **panel** (its hint), not the transcript block (which shows the prompt too).
    assert!(
        pty.wait_for_screen("space select", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(&[0x1b]); // the first Esc asks again
    assert!(
        pty.wait_for_screen("press Esc again", wait_positive()),
        "the first Esc must confirm: {}",
        pty.screen()
    );
    pty.send(&[0x1b]); // the second Esc declines
    assert!(
        pty.wait_for_screen("DECLINE-DONE", wait_positive()),
        "the turn must continue after the decline: {}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0024 R2/R3 + feedback: the `Other` row keeps its text while Up/Down
/// move between the options, and the answer carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_ask_user_other_keeps_its_text() {
    let _serial = pty_guard();
    let dir = TempDir::new("ask-other");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let questions = json!({"questions": [
        {"id": "pick", "header": "Pick", "prompt": "Pick one", "kind": "single",
         "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}],
         "allow_other": true, "required": true}
    ]});
    let server = MockServer::start(vec![
        tool_reply("call_ask", "ask_user", &questions.to_string()),
        text_reply("OTHER-DONE"),
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
            "ask-other",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        60,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    // Wait for the **panel** (its hint), not the transcript block (which shows the prompt too).
    assert!(
        pty.wait_for_screen("space select", wait_positive()),
        "{}",
        pty.screen()
    );
    // Move to `Other` (A, B, Other) and type.
    pty.send(b"\x1b[B"); // Down
    pty.send(b"\x1b[B"); // Down
    pty.send(b"custom");
    assert!(
        pty.wait_for_screen("Other: custom", wait_positive()),
        "the typed text must be in the Other row: {}",
        pty.screen()
    );
    // Up to B, Down back to Other: the text is still there.
    pty.send(b"\x1b[A"); // Up
    pty.send(b"\x1b[B"); // Down
    assert!(
        pty.wait_for_screen("Other: custom", wait_positive()),
        "the text must survive the arrows: {}",
        pty.screen()
    );
    // Enter confirms and advances to Submit; Enter submits.
    pty.send(b"\r");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("OTHER-DONE", wait_positive()),
        "the turn must continue with the Other answer: {}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("How should it load?", wait_positive()),
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
        pty.wait_until_gone("space select", wait_positive()),
        "the panel must go with the cancelled wait: {}",
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
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
        pty.wait_for_screen("1 pending", wait_positive()),
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
        pty.wait_until(wait_positive(), |screen| screen
            .contains("approve tool call?")
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
        pty.wait_for_screen("BOTH-DONE", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "no approval prompt: {}",
        pty.screen()
    );
    // The box is one widget, but the first frame after the prompt can still be a partial
    // paint under load (observed: title and first rows drawn, key line not yet). Wait for a
    // frame that has the whole key line and the window indicator, not for one snapshot.
    assert!(
        pty.wait_until(wait_positive(), |screen| screen.contains("[y] allow once")
            && screen.contains("[n] deny")
            && screen.contains("lines 1–")),
        "the key line is not fully on screen: {}",
        pty.screen()
    );
    // Scrolling the body keeps the keys on screen.
    pty.send(b"\x1b[B"); // Down
    assert!(
        pty.wait_until(wait_positive(), |screen| screen.contains("lines 2–")
            && screen.contains("[y] allow once")),
        "the box must scroll with the keys visible: {}",
        pty.screen()
    );
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("APPROVAL-FIT-RAN", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"one\r");
    assert!(
        pty.wait_for_screen("FIRST-PROVIDER-OK", wait_positive()),
        "the first turn goes to the startup provider: {}",
        pty.screen()
    );
    // The switch: the anthropic descriptor's endpoint comes from the credential's env. The
    // first Enter accepts the slash completion (the owner's rule: Enter picks, never runs);
    // the second submits the line.
    pty.send(b"/model anthropic/claude-x\r\r");
    assert!(
        pty.wait_for_screen("next turn: anthropic/claude-x", wait_positive()),
        "the status bar shows the queued switch: {}",
        pty.screen()
    );
    pty.send(b"two\r");
    assert!(
        pty.wait_for_screen("SWITCHED-PROVIDER-OK", wait_positive()),
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
        pty.wait_until(wait_positive(), |screen| !screen
            .contains("next: anthropic/claude-x")),
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

fn pty_base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// Writes a `chatgpt` `oauth` entry (ADR 0026) directly — `write_auth_json` only
/// knows the `api_key` shape, so a test that wants a `chatgpt` login writes it itself, at the
/// same `RADIAN_HOME` it passes through `envs`.
fn write_chatgpt_oauth_entry(radian_home: &Path) {
    let header = pty_base64url(br#"{"alg":"none"}"#);
    let payload =
        pty_base64url(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-pty-test"}}"#);
    let access = format!("{header}.{payload}.sig");
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 3_600_000;
    let dir = radian_home.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.join("auth.json");
    let body = serde_json::json!({
        "chatgpt": {"type": "oauth", "access": access, "refresh": "refresh-unused", "expires": expires}
    });
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// ADR 0026 slice 3: once there is a `chatgpt` login, `/model` lists its built-in
/// models alongside every other descriptor's (the flat list is unconditional for a built-in -
/// the same as `anthropic`/`openai`/`opencode-go` already are - so this also pins that the
/// new descriptor did not accidentally gain a login gate the others do not have).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_model_lists_chatgpt_once_it_is_logged_in() {
    let _serial = pty_guard();
    let dir = TempDir::new("model-chatgpt");
    let sessions = dir.path().join("sessions");
    let radian_home = dir.path().join("radian-home");
    write_chatgpt_oauth_entry(&radian_home);
    let server = MockServer::start(vec![text_reply("OK")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "model-chatgpt",
        ],
        &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/model chatgpt/");
    assert!(
        pty.wait_for_screen("chatgpt/gpt-5.4", wait_positive()),
        "the chatgpt models did not appear in the flat list:\n{}",
        pty.screen()
    );
    pty.send(b"\x1b");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// ADR 0026: switching to a `chatgpt` model with `/model` is accepted once there is
/// a `chatgpt` login. The login is an `oauth` entry, which `resolve_key` (api keys only) does
/// not see; the switch's credential check must also accept `has_login` (owner report
/// 2026-10-16: "no credential for `chatgpt`: run /login chatgpt" right after logging in).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_model_switches_to_chatgpt_with_an_oauth_login() {
    let _serial = pty_guard();
    let dir = TempDir::new("model-chatgpt-switch");
    let sessions = dir.path().join("sessions");
    let radian_home = dir.path().join("radian-home");
    write_chatgpt_oauth_entry(&radian_home);
    let server = MockServer::start(vec![text_reply("OK")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "model-chatgpt-switch",
        ],
        &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/model chatgpt/gpt-5.4\r");
    assert!(
        pty.wait_for_screen("next turn: chatgpt/gpt-5.4", wait_positive()),
        "the switch to chatgpt was refused or never confirmed:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("no credential"),
        "an oauth login must count as a credential:\n{}",
        pty.screen()
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("OPENAI-RESUME", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/model anthropic/claude-x\r\r");
    assert!(
        pty.wait_for_screen("next turn: anthropic/claude-x", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"again\r");
    assert!(
        pty.wait_for_screen("ANTHROPIC-RESUME", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"third\r");
    assert!(
        pty.wait_for_screen("RESUMED-ON-ANTHROPIC", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/thinking medium\r");
    assert!(
        pty.wait_for_screen("medium → high", wait_positive()),
        "the clamp is named: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("next turn: thinking high", wait_positive()),
        "the clamped level is queued: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("THINKING-OK", wait_positive()),
        "{}",
        pty.screen()
    );
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
        pty.wait_for_screen("switched", wait_positive()),
        "the picker did not list the session: {}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "the picked session did not open: {}",
        pty.screen()
    );
    // The status bar names the journal's model, not the config's.
    assert!(
        pty.wait_until(wait_positive(), |screen| screen.contains("claude-x")),
        "the status bar must show the journal's model: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("JOURNAL-MODEL", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("ask_user: on", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("ask_user: on", wait_positive()),
        "the startup line must say ask_user is on: {}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("NO-WS-DONE", wait_positive()),
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
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(b"/mem");
    assert!(
        pty.wait_for_screen("/memory", wait_positive()),
        "the command list did not appear:\n{}",
        pty.screen()
    );
    pty.send(b"\t");
    assert!(
        pty.wait_for_screen("❯ /memory ", wait_positive()),
        "Tab did not complete the command name:\n{}",
        pty.screen()
    );
    // The next level's list is already up; a partial is fuzzy-matched and completed.
    pty.send(b"fo");
    assert!(
        pty.wait_for_screen("forget", wait_positive()),
        "the subcommand list did not appear:\n{}",
        pty.screen()
    );
    pty.send(b"\t");
    assert!(
        pty.wait_for_screen("❯ /memory forget", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hi\r");
    assert!(
        pty.wait_for_screen("OK", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert_eq!(
        status.code(),
        Some(2),
        "a startup refusal is a usage/setup error (ADR 0032 A3)"
    );
    // The refusal is written just before the exit, so it is still in the pty buffer when
    // `try_wait` succeeds: breaking out of the loop above without draining loses it (on Linux
    // the fast exit always wins the race, and the assertion below sees an empty buffer). The
    // child is dead and holds the only slave fd, so no more bytes can arrive: one drain pass
    // until the master reports EOF/EIO collects everything already queued, with no wait.
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => drained.extend_from_slice(&buf[..n]),
            // Drained (`EAGAIN`) or the slave is closed and the buffer is empty (`EIO` on
            // Linux): both end the drain.
            Err(_) => break,
        }
    }
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
        pty.wait_for_screen("alpha", wait_positive()),
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
        pty.wait_until(wait_positive(), |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    pty.expand_startup();
    assert!(
        pty.wait_for_screen("resumed session beta", wait_positive()),
        "Enter did not resume the chosen session:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("BETA-HISTORY", wait_positive()),
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
        pty.wait_for_screen("alpha", wait_positive()),
        "the picker did not open again:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "Esc did not leave"
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"COLOR-PROBE\r");
    assert!(
        pty.wait_for_screen("REPLY-DONE", wait_positive()),
        "{}",
        pty.screen()
    );

    // The user entry's `❯ ` marker carries the theme's `user` style (the body text is the
    // terminal default by design), so check the marker cell: two columns before the probe.
    // The kanagawa fixture's `userMessageText` is `vars.fg = #DCD7BA`.
    let expected = vt100::Color::Rgb(0xDC, 0xD7, 0xBA);
    let screen = pty.parser.screen();
    let mut matched = 0;
    for (row, text) in screen.rows(0, 110).enumerate() {
        for at in text.match_indices("COLOR-PROBE").map(|(i, _)| i) {
            // The input line echoes the probe without a marker; only the transcript entry
            // has "❯ " before it. `at` is a **byte** offset (`❯` is three bytes), so the
            // marker's display column is the width of the prefix minus the two marker cells.
            let prefix_width = unicode_width::UnicodeWidthStr::width(&text[..at]) as u16;
            let Some(marker_col) = prefix_width.checked_sub(2) else {
                continue;
            };
            if let Some(cell) = screen.cell(row as u16, marker_col) {
                assert_eq!(cell.contents(), "❯", "expected the user marker");
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(b"/login custom\r");
    assert!(
        pty.wait_for_screen("provider name", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"corp\r");
    assert!(
        pty.wait_for_screen("wire protocol", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"\r"); // openai-compatible (the highlighted row)
    assert!(
        pty.wait_for_screen("base URL", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(format!("{verify}\r").as_bytes());
    assert!(
        pty.wait_for_screen("API key", wait_positive()),
        "{}",
        pty.screen()
    );
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
        pty.wait_until(wait_positive(), |screen| screen.contains(&stars)),
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
        pty.wait_for_screen("saved the key for `corp`", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(b"/login custom\r");
    assert!(
        pty.wait_for_screen("provider name", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"corp\r");
    assert!(
        pty.wait_for_screen("wire protocol", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("base URL", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(format!("{dead}\r").as_bytes());
    assert!(
        pty.wait_for_screen("API key", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(format!("{KEY}\r").as_bytes());
    assert!(
        pty.wait_for_screen("verification failed", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"y"); // save anyway
    assert!(
        pty.wait_for_screen("saved the key for `corp`", wait_positive()),
        "{}",
        pty.screen()
    );

    // `/logout corp`: confirm with `y`; the removal names the provider, the other entry and
    // the file mode survive.
    pty.send(b"/logout corp\r");
    assert!(
        pty.wait_for_screen("remove the stored credential for `corp`?", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("removed the stored credential for `corp`", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/logout existing\r");
    assert!(
        pty.wait_for_screen(
            "remove the stored credential for `existing`?",
            wait_positive()
        ),
        "{}",
        pty.screen()
    );
    pty.send(b"n");
    assert!(
        pty.wait_for_screen("kept `existing`", wait_positive()),
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
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"please write\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
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
        pty.wait_for_screen("SWITCH-DONE", wait_positive()),
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
            "--profile",
            "workspace",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("AUTO", wait_positive()),
        "the resumed session must show AUTO:\n{}",
        pty.screen()
    );
    // The startup line is one of the folded lines.
    pty.expand_startup();
    // Wait for the line itself: expand_startup returns on the first expanded frame it sees,
    // and under load the pty can hold only part of that frame (gate run, load ~300).
    assert!(
        pty.wait_for_screen(
            "auto: tool calls are approved without asking",
            wait_positive()
        ),
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
            wait_positive()
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
        pty.wait_for_screen("ignored under the full tier", wait_positive()),
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
            "--profile",
            "workspace",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("AUTO", wait_positive()),
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
        if let Some(at) = line.find(needle) {
            // `at` is a byte offset; the mouse reports display columns, so convert (a `❯ ` or
            // a CJK prefix makes the two differ).
            let col = unicode_width::UnicodeWidthStr::width(&line[..at]) as u16;
            return Some((row, col));
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
        pty.wait_for_screen("SELECT-ME-PLEASE", wait_positive()),
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
        pty.wait_for_raw(&expected, wait_positive()),
        "no OSC 52 with the selection: {}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("copied 16 chars (via OSC 52)", wait_positive()),
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
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The startup frames' cursor-show bytes are written after the text `wait_for_screen`
    // matched; settle first, so the baseline cannot miss them and blame the motion under load.
    let before = settled_frame_count(&mut pty);

    // A pointer wandering across the screen, buttons up, for a while.
    for step in 0..40u16 {
        pty.send(&sgr(35, 5 + step, 3 + step % 10, true));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let after = settled_frame_count(&mut pty);
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
        pty.wait_for_screen("type a message", wait_positive()),
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

// ---------- herdr reporting (ADR 0034) ----------

/// A fake herdr socket: accepts connections, records each request line, and replies so the
/// sender finishes. Reporting never touches the real herdr in these tests.
struct HerdrSocket {
    path: PathBuf,
    lines: Arc<Mutex<Vec<serde_json::Value>>>,
    _dir: TempDir,
}

impl HerdrSocket {
    fn start(label: &str) -> Self {
        use std::io::BufRead as _;
        use std::os::unix::net::UnixListener;
        let dir = TempDir::new(label);
        // The AF_UNIX path limit: a short `/tmp` name, not the (long) temp dir.
        let path = PathBuf::from(format!(
            "/tmp/radian-herdr-pty-{}-{label}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind the fake herdr socket");
        let lines: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&lines);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                let mut line = String::new();
                let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                if let Ok(value) = serde_json::from_str(line.trim()) {
                    log.lock().unwrap().push(value);
                }
                let _ = conn.write_all(b"{\"id\":\"x\",\"result\":{}}\n");
            }
        });
        Self {
            path,
            lines,
            _dir: dir,
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HERDR_ENV", "1".to_owned()),
            ("HERDR_PANE_ID", "w1:p1".to_owned()),
            (
                "HERDR_SOCKET_PATH",
                self.path.to_string_lossy().into_owned(),
            ),
        ]
    }

    fn snapshot(&self) -> Vec<serde_json::Value> {
        self.lines.lock().unwrap().clone()
    }

    /// Waits for a `pane.report_agent` with `state`.
    fn wait_for_state(&self, state: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.snapshot().iter().any(|value| {
                value["method"] == "pane.report_agent" && value["params"]["state"] == state
            }) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for any request of `method`.
    fn wait_for_method(&self, method: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .snapshot()
                .iter()
                .any(|value| value["method"] == method)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Every request of `method`, in arrival order.
    fn of_method(&self, method: &str) -> Vec<serde_json::Value> {
        self.snapshot()
            .into_iter()
            .filter(|value| value["method"] == method)
            .collect()
    }

    fn wait_for_release(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .snapshot()
                .iter()
                .any(|value| value["method"] == "pane.release_agent")
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for HerdrSocket {
    fn drop(&mut self) {
        // The socket binds in `/tmp` (the AF_UNIX path limit keeps it out of `_dir`), so the
        // `TempDir` does not remove it. Remove the path here: this runs on a passing test and
        // on a panicking one (unwinding drops the future's locals), so neither leaves a
        // `radian-herdr-pty-*.sock` behind. The accept thread is detached and still holds the
        // listener fd, but the file is unlinked and the inode goes away with the process.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn herdr_env(envs: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    envs.to_vec()
}

/// One mock turn: the fake socket must see idle (startup), working (TurnStarted), idle
/// (RunFinished) and a release on exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_reports_the_turn_states_and_releases() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("HERDR-REPLY-OK")]).await;
    let dir = TempDir::new("herdr-turn");
    let sock = HerdrSocket::start("turn");
    let envs = herdr_env(&sock.env());
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "herdr-turn",
        ],
        &env_refs,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_state("idle", wait_positive()),
        "the startup idle report"
    );
    pty.send(b"hello\r");
    assert!(
        sock.wait_for_state("working", wait_positive()),
        "working on TurnStarted: {:?}",
        sock.snapshot()
    );
    assert!(
        pty.wait_for_screen("HERDR-REPLY-OK", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_state("idle", wait_positive()),
        "idle on RunFinished: {:?}",
        sock.snapshot()
    );
    pty.quit();
    assert!(
        sock.wait_for_release(wait_positive()),
        "a normal exit releases: {:?}",
        sock.snapshot()
    );
    // The first report carries the session identity and the resume command.
    let first = sock
        .snapshot()
        .into_iter()
        .find(|value| value["method"] == "pane.report_agent")
        .expect("a report");
    assert_eq!(first["params"]["source"], "radian");
    assert_eq!(first["params"]["agent"], "radian");
    assert_eq!(first["params"]["agent_session_id"], "herdr-turn");
    assert!(first["params"]["agent_session_path"].is_string());
    assert_eq!(first["params"]["resume_argv"][0], "radian");
    assert_eq!(first["params"]["resume_argv"][1], "--resume");
    assert_eq!(first["params"]["resume_argv"][2], "herdr-turn");
}

/// ADR 0034: the display metadata goes as `pane.report_metadata` - every key
/// once, then only the key that changed, with no cost, token totals, paths or command text
/// anywhere in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_reports_metadata_changes() {
    let _serial = pty_guard();
    let dir = TempDir::new("herdr-meta");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.path().join("sessions");
    let secret = "echo HERDR-METADATA-SECRET";
    let server = MockServer::start(vec![
        // An `outside_sandbox` call, so the approval this watches still waits: a
        // sandboxed `workspace` shell no longer asks since slice 1 of
        // `sandbox-auto-allow-shell`.
        tool_reply(
            "call_1",
            "shell",
            &json!({
                "command": secret,
                "outside_sandbox": true,
                "outside_sandbox_reason": "herdr metadata probe",
            })
            .to_string(),
        ),
        text_reply("HERDR-META-DONE"),
    ])
    .await;
    let sock = HerdrSocket::start("meta");
    let envs = herdr_env(&sock.env());
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "herdr-meta",
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &env_refs,
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_method("pane.report_metadata", wait_positive()),
        "the metadata report: {:?}",
        sock.snapshot()
    );
    let first = sock.of_method("pane.report_metadata").remove(0);
    assert_eq!(first["params"]["source"], "radian");
    assert_eq!(first["params"]["agent"], "radian");
    assert_eq!(first["params"]["display_agent"], "radian");
    assert_eq!(first["params"]["tokens"]["model"], "mock");
    assert_eq!(first["params"]["tokens"]["pending"], "0");
    assert!(
        first["params"]["tokens"]["tier"].is_string(),
        "the tier token: {first}"
    );
    // No `tasks` token (ADR 0043 removed in-process tasks), and no cost or token totals.
    let text = serde_json::to_string(&first).unwrap();
    for forbidden in [
        "tasks",
        "cost",
        "usage",
        "tokens_input",
        "command",
        ".jsonl",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }

    // A tool call waits for an approval: the pending count changes, and the next metadata report
    // carries that key only.
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "the approval box:\n{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_state("blocked", wait_positive()),
        "{:?}",
        sock.snapshot()
    );
    let second = sock
        .of_method("pane.report_metadata")
        .into_iter()
        .find(|value| value["params"]["tokens"]["pending"] == "1")
        .unwrap_or_else(|| panic!("a pending change: {:?}", sock.snapshot()));
    assert_eq!(
        second["params"]["tokens"].as_object().unwrap().len(),
        1,
        "only the changed key: {second}"
    );
    // Decision 2: the blocked label names the category, so the sidebar need not expand it.
    assert_eq!(second["params"]["state_labels"]["blocked"], "approval");
    assert!(
        !serde_json::to_string(&sock.snapshot())
            .unwrap()
            .contains(secret),
        "the command text must never be sent"
    );
    pty.quit();
}

/// ADR 0034: a structured question labels `blocked` as `question` - the
/// category, never the question's body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_labels_a_question_as_question() {
    let _serial = pty_guard();
    let dir = TempDir::new("herdr-question");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.path().join("sessions");
    let server = MockServer::start(ask_reply("HERDR-ASK-DONE")).await;
    let sock = HerdrSocket::start("question");
    let envs = herdr_env(&sock.env());
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "herdr-question",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &env_refs,
        60,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("How should it load?", wait_positive()),
        "the question form:\n{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_state("blocked", wait_positive()),
        "{:?}",
        sock.snapshot()
    );
    let blocked = sock
        .of_method("pane.report_metadata")
        .into_iter()
        .find(|value| value["params"]["state_labels"]["blocked"] == "question")
        .unwrap_or_else(|| panic!("a question label: {:?}", sock.snapshot()));
    assert_eq!(blocked["params"]["tokens"].as_object().unwrap().len(), 1);
    let state = sock
        .snapshot()
        .into_iter()
        .find(|value| value["params"]["state"] == "blocked")
        .expect("a blocked state report");
    assert_eq!(state["params"]["message"], "question");
    assert!(
        !serde_json::to_string(&sock.snapshot())
            .unwrap()
            .contains("How should it load?"),
        "the question body must never be sent"
    );
    pty.quit();
}

/// ADR 0034: the session identity goes as its own
/// `pane.report_agent_session`, after the state report that holds the pane (herdr refuses a
/// `resume_argv` from a reporter that does not hold the pane yet).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_reports_the_session_identity() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("HERDR-IDENTITY-OK")]).await;
    let dir = TempDir::new("herdr-identity");
    let sock = HerdrSocket::start("identity");
    let envs = herdr_env(&sock.env());
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "herdr-identity",
        ],
        &env_refs,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_method("pane.report_agent_session", wait_positive()),
        "the session identity report: {:?}",
        sock.snapshot()
    );
    let methods: Vec<String> = sock
        .snapshot()
        .iter()
        .map(|value| value["method"].as_str().unwrap_or_default().to_owned())
        .collect();
    let state_at = methods
        .iter()
        .position(|method| method == "pane.report_agent")
        .expect("a state report");
    let session_at = methods
        .iter()
        .position(|method| method == "pane.report_agent_session")
        .expect("a session report");
    assert!(state_at < session_at, "{methods:?}");
    let identity = sock.of_method("pane.report_agent_session").remove(0);
    assert_eq!(identity["params"]["pane_id"], "w1:p1");
    assert_eq!(identity["params"]["source"], "radian");
    assert_eq!(identity["params"]["agent"], "radian");
    assert_eq!(identity["params"]["agent_session_id"], "herdr-identity");
    assert_eq!(identity["params"]["resume_argv"][0], "radian");
    assert_eq!(identity["params"]["resume_argv"][1], "--resume");
    assert_eq!(identity["params"]["resume_argv"][2], "herdr-identity");
    pty.quit();
}

/// ADR 0034: the input-to-`working` path must not wait for the model's
/// first token (`herdr agent prompt --wait` gives it 5 s). The mock never sends one, so a report
/// that waited for a token would never arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_reports_working_within_a_second_of_the_input() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let dir = TempDir::new("herdr-gate");
    let sock = HerdrSocket::start("gate");
    let envs = herdr_env(&sock.env());
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "herdr-gate",
        ],
        &env_refs,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_state("idle", wait_positive()),
        "the startup idle report"
    );
    let sent = Instant::now();
    pty.send(b"hello\r");
    let early = sock.wait_for_state("working", WAIT_LATENCY_BOUND);
    assert!(
        early,
        "working within {:?}, not at the first token: {:?}",
        WAIT_LATENCY_BOUND,
        sock.snapshot()
    );
    assert!(
        sent.elapsed() < Duration::from_secs(2),
        "the working report took {:?}",
        sent.elapsed()
    );
    pty.quit();
}

/// An approval is `blocked` with the tool name only; the command text is never sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_reports_blocked_without_the_command_text() {
    let _serial = pty_guard();
    let dir = TempDir::new("herdr-approval");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.path().join("sessions");
    let command = "echo HERDR-SECRET-COMMAND";
    let server = MockServer::start(vec![
        // An `outside_sandbox` call, so the approval the herdr report watches still
        // waits: a sandboxed `workspace` shell no longer asks since slice 1 of
        // `sandbox-auto-allow-shell`.
        tool_reply(
            "call_1",
            "shell",
            &json!({
                "command": command,
                "outside_sandbox": true,
                "outside_sandbox_reason": "herdr blocked-state probe",
            })
            .to_string(),
        ),
        text_reply("HERDR-DONE"),
    ])
    .await;
    let sock = HerdrSocket::start("approval");
    let envs = herdr_env(&sock.env());
    let env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "herdr-approval",
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &env_refs,
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"go\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "the approval box:\n{}",
        pty.screen()
    );
    assert!(
        sock.wait_for_state("blocked", wait_positive()),
        "blocked while the approval waits: {:?}",
        sock.snapshot()
    );
    let blocked = sock
        .snapshot()
        .into_iter()
        .find(|value| value["params"]["state"] == "blocked")
        .expect("a blocked report");
    assert_eq!(blocked["params"]["message"], "approval: shell");
    let all = serde_json::to_string(&sock.snapshot()).unwrap();
    assert!(
        !all.contains("HERDR-SECRET-COMMAND"),
        "the command text must never be sent: {all}"
    );
    pty.quit();
}

/// Without `HERDR_ENV=1` nothing is sent at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_is_silent_without_the_env() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("HERDR-SILENT-OK")]).await;
    let dir = TempDir::new("herdr-off");
    let sock = HerdrSocket::start("off");
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "herdr-off",
        ],
        &[
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_SOCKET_PATH", sock.path.to_str().unwrap()),
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("HERDR-SILENT-OK", wait_positive()),
        "{}",
        pty.screen()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        sock.snapshot().is_empty(),
        "no HERDR_ENV means no reports: {:?}",
        sock.snapshot()
    );
    pty.quit();
}

/// `RADIAN_HERDR=0` turns it off for one run even inside a herdr pane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_herdr_is_silent_when_disabled() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("HERDR-DISABLED-OK")]).await;
    let dir = TempDir::new("herdr-disabled");
    let sock = HerdrSocket::start("disabled");
    let envs = herdr_env(&sock.env());
    let mut env_refs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    env_refs.push(("RADIAN_HERDR", "0"));
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "herdr-disabled",
        ],
        &env_refs,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("HERDR-DISABLED-OK", wait_positive()),
        "{}",
        pty.screen()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        sock.snapshot().is_empty(),
        "RADIAN_HERDR=0 means no reports: {:?}",
        sock.snapshot()
    );
    pty.quit();
}

// ---------- Ctrl-C: clear first, then press twice to quit ----------

/// Idle and empty, Ctrl-C asks twice: the first press only shows the hint and never exits;
/// the second inside the window exits 0 with the copyable resume line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_ctrl_c_twice_quits_and_one_press_only_hints() {
    let _serial = pty_guard();
    let dir = TempDir::new("ctrl-c-quit");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "ctrl-c-quit",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(&[0x03]);
    assert!(
        pty.wait_for_screen("Press Ctrl-C again to quit", wait_positive()),
        "the first press must show the hint:\n{}",
        pty.screen()
    );
    assert!(
        pty.app_exit_code().is_none(),
        "a single Ctrl-C must not quit:\n{}",
        pty.screen()
    );

    pty.send(&[0x03]);
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "the second press must quit:\n{}",
        pty.screen()
    );
    assert_eq!(pty.app_exit_code(), Some(0));

    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let after = raw
        .rsplit_once("\u{1b}[?1049l")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default();
    assert!(
        after
            .lines()
            .any(|line| line.contains("resume: radian --resume ctrl-c-quit")),
        "the exit line is missing: {after}"
    );
}

/// Idle with text, one Ctrl-C clears the input (it does not quit) and the cleared line is
/// back through `Up`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_ctrl_c_clears_a_draft_before_quitting() {
    let _serial = pty_guard();
    let dir = TempDir::new("ctrl-c-clear");
    let server = MockServer::start(vec![]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "ctrl-c-clear",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"a draft");
    assert!(
        pty.wait_for_screen("a draft", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(&[0x03]);
    // The input line is empty again and the app is still running (no quit).
    assert!(
        pty.app_exit_code().is_none(),
        "clearing must not quit:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen_absent("Press Ctrl-C again to quit"),
        "clearing must not arm the quit window:\n{}",
        pty.screen()
    );
    // `Up` recalls the cleared line from the history.
    pty.send(&[0x1b, b'[', b'A']);
    assert!(
        pty.wait_for_screen("a draft", wait_positive()),
        "the cleared line is in the history:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- /theme: switch and write back (ADR 0008 revision) ----------

/// `/theme light` end to end: the notice names the saved file, the config's other bytes
/// survive, and only `tui.theme` changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_slash_theme_switches_and_writes_the_config() {
    let _serial = pty_guard();
    let dir = TempDir::new("theme-switch");
    let config = dir.path().join("config.toml");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let original = "# kept\nschema_version = 1\n\n[tui]\ntheme = \"dark\"\n";
    std::fs::write(&config, original).unwrap();
    let server = MockServer::start(vec![]).await;
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
            "theme-switch",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/theme light");
    assert!(
        pty.wait_for_screen("/theme light", wait_positive()),
        "the line did not render:\n{}",
        pty.screen()
    );
    // Enter may pick the completion candidate first; the next Enter submits the line.
    pty.send(b"\r");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("theme saved to", wait_positive()),
        "the save notice is missing:\n{}",
        pty.screen()
    );
    let after = std::fs::read_to_string(&config).unwrap();
    assert_eq!(
        after,
        original.replace("dark", "light"),
        "only theme changed"
    );

    // An unknown name is refused and the file is untouched.
    pty.send(b"/theme nope");
    assert!(
        pty.wait_for_screen("/theme nope", wait_positive()),
        "the line did not render:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("no theme \"nope\"", wait_positive()),
        "the refusal notice is missing:\n{}",
        pty.screen()
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), after);

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// review (small-fixes 1): a key `/login` saves **mid-session** joins this session's
/// redaction list, so an export of the same session hides it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_login_key_is_redacted_from_an_export_in_the_same_session() {
    let _serial = pty_guard();
    // Deliberately not `sk-`-prefixed and not after `Bearer`: the export's heuristic
    // redaction would catch such a token on its own, and this test is about the *known*
    // secret list (the mid-session `/login`), not the heuristic.
    const KEY: &str = "late-secret-PTY-7";
    let verify = login_verify_server("200 OK");
    let dir = TempDir::new("login-redact");
    let server = MockServer::start(vec![text_reply("DONE")]).await;
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "login-redact",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );

    pty.send(b"/login custom\r");
    assert!(
        pty.wait_for_screen("provider name", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"corp\r");
    assert!(
        pty.wait_for_screen("wire protocol", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"\r"); // openai-compatible
    assert!(
        pty.wait_for_screen("base URL", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(format!("{verify}\r").as_bytes());
    assert!(
        pty.wait_for_screen("API key", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(format!("{KEY}\r").as_bytes());
    assert!(
        pty.wait_for_screen("saved the key for `corp`", wait_positive()),
        "{}",
        pty.screen()
    );

    // Put the key into the transcript, then export this same session.
    pty.send(format!("the key is {KEY}\r").as_bytes());
    assert!(
        pty.wait_for_screen("DONE", wait_positive()),
        "{}",
        pty.screen()
    );

    let out = dir.path().join("export.json");
    pty.send(format!("/export json {}\r", out.display()).as_bytes());
    assert!(
        pty.wait_for_screen("exported to", wait_positive()),
        "the export notice is missing:\n{}",
        pty.screen()
    );

    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("<redacted>"),
        "the export did not redact the mid-session key: {text}"
    );
    assert!(
        !text.contains(KEY),
        "the mid-session key leaked into the export: {text}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The default footer is pi's two rows (ADR 0027 C2): the model on the first, the tokens on
/// the second. The pty harness otherwise starts tests on `preset = classic`, so this test
/// passes the `pi` preset explicitly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_pi_footer_is_two_rows() {
    let _serial = pty_guard();
    let dir = TempDir::new("footer-pi");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "footer-pi",
        ],
        &[],
        "schema_version = 1\n[tui.footer]\npreset = \"pi\"\n",
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The model item is on the footer's first row; the tokens item on the second. The screen
    // text is one string, so both must be present, and the tokens glyph marks the second row.
    assert!(
        pty.wait_for_screen("↑-", wait_positive()),
        "the tokens row is missing:\n{}",
        pty.screen()
    );
    assert!(pty.screen().contains("mock"), "{}", pty.screen());
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The **shipped default** with no config file: the two-row `pi` footer (ADR 0027 C2).
/// `spawn_bare` writes no `[tui.footer]`, so this is exactly what a user gets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_shipped_default_footer_is_two_rows_without_a_config() {
    let _serial = pty_guard();
    let dir = TempDir::new("footer-default");
    // A short workspace so the cwd item fits at a normal width (a long path is legitimately
    // dropped by the narrow-terminal rule, which the second test covers).
    let ws = short_workspace("footer-default");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "footer-default",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[],
        30,
        90,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // Wait for the **footer** before reading its last two lines: `message` is the input hint,
    // which is drawn in the same frame as the footer, but the hint can appear before the
    // footer's items have data. `↑- ↓- R-` is the numbers row with no usage yet (the unicode
    // tier's token arrows, ADR 0027 item 2).
    assert!(
        pty.wait_for_screen("↑- ↓-", wait_positive()),
        "the numbers row is on screen:\n{}",
        pty.screen()
    );
    let lines: Vec<String> = pty.screen().lines().map(str::to_owned).collect();
    let first = lines.get(lines.len() - 2).cloned().unwrap_or_default();
    let second = lines.last().cloned().unwrap_or_default();
    // The shipped default is the `deck` preset (ADR 0027): the identity row (model,
    // cwd, branch, worktree) and the numbers row (tokens, cost, context). The `deck` identity
    // row has no brand, so there is no brand assertion (the old one passed only because the
    // temp workspace is named `radian-…`).
    assert!(
        first.contains("mock"),
        "the model is on the identity row: {first:?}"
    );
    assert!(
        first.contains("footer-default"),
        "the repo/worktree name is on the identity row: {first:?}"
    );
    assert!(
        second.contains('↑'),
        "the numbers row shows the tokens: {second:?}"
    );
    assert!(
        second.contains('%'),
        "the numbers row shows the context share: {second:?}"
    );
    assert!(
        !second.contains("mock"),
        "the model is not on the numbers row: {second:?}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// F3 (ADR 0027): `--resume` shows what the session already spent. Before this the
/// tokens were `-` and `cost` was hidden until the next response, because the projection starts
/// empty and only the live sink wrote to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_resume_shows_the_sessions_earlier_tokens_and_fee() {
    let _serial = pty_guard();
    let dir = TempDir::new("cost-resume");
    let home = TempDir::new("cost-resume-home");
    // A catalog cache with a price for the model the session ran on. `resolve_offline` reads
    // it, so the resumed fee is resolved without any network (ADR 0026 D7).
    let cache = home.path().join("cache/catalog");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("models.json"),
        serde_json::json!({
            "opencode-go": {
                "id": "opencode-go",
                "models": {
                    "deepseek-v4.1-flash": {
                        "id": "deepseek-v4.1-flash",
                        "limit": {"context": 1000000, "input": 922000, "output": 128000},
                        "cost": {"input": 0.15, "output": 0.6, "cache_read": 0.003}
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    // The journal of a session that already ran: 1M input on the priced model = $0.15.
    let path = dir.path().join("resumed.jsonl");
    {
        let journal = radian_session::SessionJournal::create(
            &path,
            "resumed",
            radian_session::SessionCreatedData::new(
                "/tmp",
                serde_json::json!({"model": {"provider": "opencode-go", "model": "deepseek-v4.1-flash"}}),
            ),
        )
        .unwrap();
        use agent_core::TranscriptJournal;
        journal
            .append(agent_core::JournalRecord::Message {
                index: 0,
                message: agent_core::Message::user("earlier turn"),
            })
            .await
            .unwrap();
        journal
            .append(agent_core::JournalRecord::Message {
                index: 1,
                message: agent_core::Message::assistant("earlier answer"),
            })
            .await
            .unwrap();
        journal
            .append(agent_core::JournalRecord::ModelUsage {
                usage: agent_core::ModelUsage {
                    input: Some(1_000_000),
                    output: Some(100_000),
                    input_includes_cache: Some(true),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        drop(journal);
    }

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "deepseek-v4.1-flash",
            "--provider",
            "opencode-go",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--resume",
            "resumed",
        ],
        &[
            ("RADIAN_HOME", home.path().to_str().unwrap()),
            // The harness writes the `opencode-go` credential into `auth.json` from this env
            // name (ADR 0026 D2: a key comes only from `auth.json`).
            ("OPENCODE_GO_API_KEY", "test-key-not-a-secret"),
        ],
        // The numbers row, so `tokens` and `cost` are both on screen.
        "schema_version = 1\n[tui.footer]\nrows = [{ left = [] }, { left = [\"tokens\", \"cost\"] }]\n",
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The earlier turn's tokens are there (1M in, 100K out). The default `tokens` format is
    // `↑{in} ↓{out} R{cache}` (ADR 0027 item 2's unicode arrows), and `format_tokens`
    // keeps one decimal under 10 (so 1 000 000 is `1.0M`) and rounds above it (`100K`).
    assert!(
        pty.wait_for_screen("↑1.0M", wait_positive()),
        "the resumed token total is missing:\n{}",
        pty.screen()
    );
    assert!(pty.screen().contains("↓100K"), "{}", pty.screen());
    // ... and its fee: 1M x $0.15 + 100K x $0.60 per Mtok = $0.21, three decimals
    // (ADR 0027).
    assert!(
        pty.wait_for_screen("$0.210", wait_positive()),
        "the resumed fee is missing:\n{}",
        pty.screen()
    );
    let _ = server;
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The shipped default on a narrow terminal follows the drop order: the `right` group
/// (reasoning) goes before the `left` group's end, and the model (highest priority) stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_the_default_footer_drops_the_right_group_on_a_narrow_terminal() {
    let _serial = pty_guard();
    let dir = TempDir::new("footer-narrow");
    let ws = short_workspace("footer-narrow");
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "footer-narrow",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[],
        30,
        // Narrow enough that the right group cannot fit, wide enough for the model.
        22,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    let lines: Vec<String> = pty.screen().lines().map(str::to_owned).collect();
    let first = lines.get(lines.len() - 2).cloned().unwrap_or_default();
    let second = lines.last().cloned().unwrap_or_default();
    assert!(first.contains("mock"), "the model survives: {first:?}");
    assert!(
        !first.contains('*'),
        "the right group drops first: {first:?}"
    );
    assert!(
        !second.contains("mock"),
        "the tokens row is still the second: {second:?}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// A short workspace directory under `/tmp`, so the footer's cwd item fits a narrow terminal
/// (the long per-test temp path would be dropped by the truncation rule).
fn short_workspace(label: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(format!("/tmp/radian-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// The longest run of `ch` on any screen line: the input box's rule length is a cheap proxy
/// for "the app laid out at this width".
fn longest_run(screen: &str, ch: char) -> usize {
    screen
        .lines()
        .map(|line| {
            line.chars()
                .fold((0usize, 0usize), |(best, run), c| {
                    if c == ch {
                        (best.max(run + 1), run + 1)
                    } else {
                        (best, 0)
                    }
                })
                .0
        })
        .max()
        .unwrap_or(0)
}

/// changing the window size with **no other input** must re-lay out. The
/// old bug: crossterm only drains its SIGWINCH pipe while reading stdin, so an idle app stayed
/// at the old width until a key or wheel arrived ("scroll once and it fixes itself"). The app
/// now polls `TIOCGWINSZ` on its idle tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_bare_resize_re_lays_out() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("never")]).await;
    // The rule is the width witness, so this test asks for `tui.input_style = "lines"`: the
    // default `bar` band has no rule to measure (its width at 80 and 120 columns is pinned by
    // the `TestBackend` snapshots in `tests/render.rs`).
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n[tui]\ninput_style = \"lines\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 100);
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    let before = longest_run(&pty.screen(), '─');
    assert!(before > 90, "the initial rule spans the terminal: {before}");

    // Only the size changes; no key, no wheel.
    pty.resize(24, 60);
    assert!(
        pty.wait_until(wait_positive(), |screen| longest_run(screen, '─') < 62),
        "no re-layout after a bare resize (rule was {before}, now {}):\n{}",
        longest_run(&pty.screen(), '─'),
        pty.screen()
    );

    // Three changes in a row land on the last size.
    pty.resize(24, 70);
    pty.resize(24, 80);
    pty.resize(24, 50);
    assert!(
        pty.wait_until(wait_positive(), |screen| longest_run(screen, '─') < 52),
        "the final size must win (rule is {}):\n{}",
        longest_run(&pty.screen(), '─'),
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// The keyboard-enhancement flags (kitty protocol, ): when the terminal
/// supports them, `DISAMBIGUATE_ESCAPE_CODES` is pushed so Shift+Enter arrives as
/// Enter+SHIFT instead of a plain Enter, and the flags are popped on **every** exit path.
///
/// A pty cannot answer the support query, so this test forces the push through the
/// `RADIAN_KEYBOARD_ENHANCEMENT=1` override; the query itself is a real-terminal matter (the
/// hand-test list). The push is `CSI > 1 u` (DISAMBIGUATE_ESCAPE_CODES = 1) and the pop is
/// `CSI < 1 u` (crossterm 0.29.0 `event.rs:495-527`).
#[test]
fn pty_keyboard_enhancement_is_pushed_and_popped_on_exit() {
    let _serial = pty_guard();
    let dir = TempDir::new("keyboard-enhance");
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "keyboard-enhance",
            "--offline-catalog",
        ],
        &[("RADIAN_KEYBOARD_ENHANCEMENT", "1")],
        20,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_raw("\u{1b}[>1u", wait_positive()),
        "the enhancement flags are pushed at startup"
    );
    pty.quit();
    let raw = String::from_utf8_lossy(&pty.raw()).to_string();
    let pop = raw
        .find("\u{1b}[<1u")
        .expect("the flags are popped on exit");
    let leave = raw
        .rfind("\u{1b}[?1049l")
        .expect("the alternate screen is left");
    assert!(
        pop < leave,
        "the pop comes before leaving the alternate screen"
    );
}

/// ADR 0033 acceptance: `!cmd` runs in the sandbox, shows a `$ cmd` block, and its `Ran \`cmd\``
/// message reaches the next request's user messages.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bang_runs_and_enters_the_context() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("AFTER-BANG")]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 100);
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The output must not be a substring of the command text: `wait_for_screen` would
    // otherwise match the echoed input line and race the command (the audit-test flake).
    pty.send(b"!printf 'BANG-%s' OUTPUT\r");
    assert!(
        pty.wait_for_screen("BANG-OUTPUT", wait_positive()),
        "the command output is not on screen:\n{}",
        pty.screen()
    );
    // The block's first line is `$ cmd`.
    assert!(
        pty.wait_for_screen("$ printf 'BANG-%s' OUTPUT", wait_positive()),
        "the command line is not on screen:\n{}",
        pty.screen()
    );
    // ADR 0033 + ADR 0012: `!` is the human's own command; it never opens the approval box.
    assert!(
        !pty.screen().contains("approve tool call"),
        "`!` must not ask for approval:\n{}",
        pty.screen()
    );
    pty.send(b"next\r");
    assert!(
        pty.wait_for_screen("AFTER-BANG", wait_positive()),
        "the reply is missing:\n{}",
        pty.screen()
    );
    let body = server.requests()[0].json();
    let users = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(users.contains("Ran `printf 'BANG-%s' OUTPUT`"), "{users}");
    assert!(users.contains("BANG-OUTPUT"), "{users}");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0033 acceptance: `!!cmd` runs and shows, but its output never enters the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bangbang_stays_out_of_the_context() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("AFTER-BANGBANG")]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 100);
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // Distinct output (the command text has no `BANG-SECRET`), so the wait cannot match the
    // echoed input line.
    pty.send(b"!!printf 'BANG-%s' SECRET\r");
    assert!(
        pty.wait_for_screen("BANG-SECRET", wait_positive()),
        "the command output is not on screen:\n{}",
        pty.screen()
    );
    pty.send(b"next\r");
    assert!(
        pty.wait_for_screen("AFTER-BANGBANG", wait_positive()),
        "the reply is missing:\n{}",
        pty.screen()
    );
    let body = server.requests()[0].json();
    let users = body["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !users.contains("BANG-SECRET"),
        "!! output must not enter the context: {users}"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0033 + ADR 0012: a `!` command really is confined — a write outside the workspace
/// fails under `--profile workspace`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bang_is_confined_to_the_workspace() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let escape = format!("/tmp/radian-bang-escape-{}", std::process::id());
    let _ = std::fs::remove_file(&escape);
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(
        &["--profile", "workspace"],
        &[("OPENAI_API_KEY", "sk-test")],
        &config,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(format!("!echo x > {escape}\r").as_bytes());
    assert!(
        pty.wait_for_screen("[exit code:", wait_positive()),
        "the block never appeared:\n{}",
        pty.screen()
    );
    assert!(
        !std::path::Path::new(&escape).exists(),
        "the sandbox must deny a write outside the workspace"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0033: every `!`/`!!` run is audited in the journal (`UserBash`), even when it does not
/// enter the context.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bang_is_audited_in_the_journal() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let dir = TempDir::new("bang-audit");
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(
        &["--session-dir", dir.path().to_str().unwrap()],
        &[("OPENAI_API_KEY", "sk-test")],
        &config,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The output must not appear in the command text: `wait_for_screen("AUDITED")` used to
    // match the echoed `!!printf AUDITED` and quit before the command (and its `UserBash`
    // record) ran. `printf 'A%sB' Z` prints `AZB`, which the command text does not contain.
    pty.send(b"!!printf 'A%sB' Z\r");
    assert!(
        pty.wait_for_screen("AZB", wait_positive()),
        "the command output is not on screen:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    let journal = std::fs::read_dir(dir.path())
        .expect("the session dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .expect("a session file");
    let text = std::fs::read_to_string(&journal).expect("the journal is readable");
    assert!(
        text.contains("\"event\":\"UserBash\""),
        "no UserBash audit record:\n{text}"
    );
    assert!(
        text.contains("\"command\":\"printf 'A%sB' Z\"")
            && text.contains("\"exclude_from_context\":true"),
        "the audit record is missing its fields:\n{text}"
    );
}

/// ADR 0033: only one `!` at a time (a second is refused) and Esc cancels the running one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bang_is_single_and_escapable() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 100);
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // A running `!` has no block yet (the block appears when it finishes), and the composer
    // echo is gone as soon as Enter is handled, so there is nothing to wait for here: the two
    // sends are processed in order, so the second sees the first already running.
    pty.send(b"!sleep 30\r");
    // A second command while the first runs is refused, not queued or run.
    pty.send(b"!echo SECOND\r");
    assert!(
        pty.wait_for_screen("already running", wait_positive()),
        "the second command was not refused:\n{}",
        pty.screen()
    );
    // Esc cancels the running one; its block says so.
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_screen("[cancelled]", wait_positive()),
        "the command was not cancelled:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0033: Ctrl-C (`Effect::Cancel`) also stops a running `!`, so there is no orphan process
/// group. A turn must be running for Ctrl-C to mean "cancel" (otherwise it arms quit), so this
/// starts one with a reply that never finishes and then cancels both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_ctrl_c_stops_a_running_bang() {
    let _serial = pty_guard();
    // A reply that never finishes: the turn stays running while the bash does.
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 100);
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // No block to wait for (it appears when the command finishes); the sends are ordered, so
    // the `!` is running by the time the turn below starts.
    pty.send(b"!sleep 30\r");

    // Start a turn so Ctrl-C is a cancel; the mock hangs, so it is still running.
    pty.send(b"hello\r");
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.requests().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !server.requests().is_empty(),
        "the turn never started:\n{}",
        pty.screen()
    );

    // Ctrl-C cancels the turn AND the running `!`.
    pty.send(&[0x03]);
    assert!(
        pty.wait_for_screen("[cancelled]", wait_positive()),
        "Ctrl-C did not cancel the running !:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0033: the loop runs `!` through radian-coding's **sandboxed** executor, never a naked
/// `Command`. A structural test pins it, so a future edit cannot quietly bypass the sandbox.
#[test]
fn the_bang_command_runs_through_the_sandboxed_executor() {
    skip_inside_sandbox!("the_bang_command_runs_through_the_sandboxed_executor");
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/cli/loop_context.rs"
    ))
    .unwrap();
    assert!(
        source.contains("radian_coding::run_user_bash"),
        "the loop must reuse the shell tool's sandboxed executor"
    );
    let arm = source
        .split("Effect::RunBash")
        .nth(1)
        .unwrap_or_default()
        .split("Effect::Inject")
        .next()
        .unwrap_or_default();
    assert!(
        !arm.contains("Command::new"),
        "the RunBash arm must not build a command directly (that would bypass the sandbox):\n{arm}"
    );
}

/// ADR 0038 slice 2: switching to `full` (no sandbox) opens a confirmation; Esc cancels and
/// leaves the tier unchanged. The model has no tool for this, so it is a user-only command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_access_full_requires_confirmation_and_esc_cancels() {
    let _serial = pty_guard();
    let dir = TempDir::new("access-confirm");
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
            dir.path().join("sessions").to_str().unwrap(),
            "--session-id",
            "access-confirm",
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("workspace", wait_positive()),
        "the startup must show the tier:\n{}",
        pty.screen()
    );

    // `/access full` opens the confirmation; nothing has switched yet.
    pty.send(b"/access full\r");
    assert!(
        pty.wait_for_screen("full means no sandbox", wait_positive()),
        "the confirmation must be up:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("shows no prompts", wait_positive()),
        "the confirmation must say nothing prompts:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("access: full"),
        "the tier must not switch before confirmation:\n{}",
        pty.screen()
    );

    // Esc cancels.
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_screen("access unchanged", wait_positive()),
        "Esc must cancel:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("access: full"),
        "the tier must be unchanged after Esc:\n{}",
        pty.screen()
    );

    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0038 decision 3: a remembered per-project `full` applies at the next start, and the
/// startup line and the footer mark it `(project)`. A file **in the workspace** cannot cause
/// it — only the user's `~/.radian/access/access.json` can.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_remembered_full_applies_at_start_with_the_project_marker() {
    let _serial = pty_guard();
    let dir = TempDir::new("access-remembered");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The store is keyed by the ADR 0021 project identity, and `RADIAN_HOME` is the root the
    // app reads (not `<root>/.radian`): `$RADIAN_HOME/access/access.json`.
    let key = radian_host::memory::project_identity_key(&ws)
        .to_string_lossy()
        .into_owned();
    let home = dir.path().join("home");
    let store_dir = home.join("access");
    std::fs::create_dir_all(&store_dir).unwrap();
    std::fs::write(
        store_dir.join("access.json"),
        format!("{{\"{key}\": \"full\"}}\n"),
    )
    .unwrap();

    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().join("sessions").to_str().unwrap(),
            "--session-id",
            "access-remembered",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.to_str().unwrap())],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("full · no prompts (project)", wait_positive()),
        "the remembered tier must apply and be marked:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));

    // A workspace file with the same content, and a fresh HOME with no store, must not set the
    // tier: a cloned repository cannot ship a line that opens (or narrows) the sandbox. The
    // built-in default is `full` (ADR 0043).
    let home2 = dir.path().join("home2");
    std::fs::create_dir_all(&home2).unwrap();
    std::fs::create_dir_all(ws.join(".radian")).unwrap();
    std::fs::write(
        ws.join(".radian/access.json"),
        format!("{{\"{key}\": \"full\"}}\n"),
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;
    let mut pty = Pty::spawn_with_env(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().join("sessions2").to_str().unwrap(),
            "--session-id",
            "access-wsfile",
            "--workspace",
            ws.to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home2.to_str().unwrap())],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("no sandbox", wait_positive()),
        "the default tier must be `full`: {:?}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("(project)"),
        "a workspace file must not set the tier:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// ADR 0038 decision 2: a journaled access change restores the tier on resume, the same way
/// `/model` restores the route.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_resume_restores_the_access_tier() {
    let _serial = pty_guard();
    let dir = TempDir::new("access-resume");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("access-resume.jsonl");
    let journal = radian_session::SessionJournal::create(
        &path,
        "access-resume",
        radian_session::SessionCreatedData::new(ws.to_string_lossy(), json!({})),
    )
    .expect("create the session file");
    journal
        .append_runtime_event(radian_session::RuntimeEvent::ConfigChanged {
            revision: 1,
            change: json!({"access": "full"}),
        })
        .await
        .unwrap();
    drop(journal);

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
            "--resume",
            "access-resume",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("full (session)", wait_positive()),
        "the resumed tier must apply and be marked:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

// ---------- ADR 0043: `radian attach` ----------

/// The journal the attach tests read, returned with its writer **still open**: the appender
/// really is a different process from the reader, and the writer's lock is held the whole time
/// (attach never takes it — a reader that locked would block here). The messages are
/// `ATTACH-OLD-USER`/`ATTACH-OLD-REPLY` followed by `extra` filler replies.
async fn attach_session(dir: &Path, id: &str, extra: usize) -> radian_session::SessionJournal {
    use agent_core::TranscriptJournal;
    let path = dir.join(format!("{id}.jsonl"));
    let journal = radian_session::SessionJournal::create(
        &path,
        id,
        radian_session::SessionCreatedData::new(dir.to_string_lossy().into_owned(), json!({})),
    )
    .expect("create the session file");
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: agent_core::Message::user("ATTACH-OLD-USER"),
        })
        .await
        .unwrap();
    journal
        .append(agent_core::JournalRecord::Message {
            index: 1,
            message: agent_core::Message::assistant("ATTACH-OLD-REPLY"),
        })
        .await
        .unwrap();
    for index in 0..extra {
        journal
            .append(agent_core::JournalRecord::Message {
                index: index + 2,
                message: agent_core::Message::assistant(format!("ATTACH-FILLER-{index}")),
            })
            .await
            .unwrap();
    }
    journal
}

/// The entries of `dir`, sorted; empty when the directory does not exist.
fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// `run/` under the app's `RADIAN_HOME`: where a control-port socket would appear. attach must
/// never create one (it reads the journal directly, ADR 0043 D15).
fn run_entries(home: &Path) -> Vec<String> {
    dir_entries(&home.join("run"))
}

/// The `.jsonl` files of `dir` with their bytes, by name: the read-only checks compare this
/// before and after a run. The picker's own `index.db` cache is not a session file and is
/// deliberately not compared (ADR 0006 §10: the index is a disposable cache over the JSONL).
fn jsonl_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&path).unwrap())
        })
        .collect();
    files.sort();
    files
}

/// The `--resume <id>` hint the picker draws on its top border: the **selected** row's id.
fn picker_hint(screen: &str) -> Option<String> {
    let rest = screen.split_once("--resume ")?.1;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    (!id.is_empty()).then_some(id)
}

/// A1: `attach --follow` shows the committed messages of a journal **another process** is
/// appending, and follows a new line within a second. The second turn's `working…` line comes
/// from the durable `TurnStarted` and leaves with its `TurnFinished`.
///
/// Mutation: drop the follow (read the file once, never poll the tail again) → the new message
/// and `working…` never arrive and the disappearance is never seen → FAILED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_follow_shows_new_committed_events() {
    let _serial = pty_guard();
    use agent_core::TranscriptJournal;
    let dir = TempDir::new("attach-follow");
    let journal = attach_session(dir.path(), "attach-me", 0).await;

    let mut pty = Pty::spawn(
        &[
            "attach",
            "attach-me",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--follow",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("ATTACH-OLD-REPLY", wait_positive()),
        "the committed history is not on screen:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("attach · attach-me · read-only", wait_positive()),
        "the attach footer is not on screen:\n{}",
        pty.screen()
    );

    // A second turn opens; its first committed message follows the `TurnStarted`.
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnStarted {
            turn_id: "t-2".to_owned(),
        })
        .await
        .unwrap();
    journal
        .append(agent_core::JournalRecord::Message {
            index: 2,
            message: agent_core::Message::assistant("ATTACH-FOLLOW-NEW"),
        })
        .await
        .unwrap();
    assert!(
        pty.wait_for_screen("ATTACH-FOLLOW-NEW", WAIT_LATENCY_BOUND),
        "the appended message did not arrive within a second:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("working…", WAIT_LATENCY_BOUND),
        "the open turn's `working…` line did not arrive within a second:\n{}",
        pty.screen()
    );

    // The turn finishes: the durable line goes away.
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnFinished {
            turn_id: "t-2".to_owned(),
            outcome: radian_session::TurnOutcome::Completed,
            reason: None,
            stop_reason: None,
        })
        .await
        .unwrap();
    assert!(
        pty.wait_until_gone("working…", WAIT_LATENCY_BOUND),
        "the finished turn still shows `working…`:\n{}",
        pty.screen()
    );

    pty.send(b"q");
    assert_eq!(
        pty.wait_for_exit(wait_positive()),
        Some(0),
        "q must quit the view"
    );
}

/// A session id becomes a file name, so a path-like id must be refused before anything is read:
/// one level above the session directory sits a real, readable session (the decoy), and its
/// text must never reach the screen. The refusal is the same rule `--resume` applies
/// (`validate_session_id`).
///
/// Mutation: drop the validation and build the path from the raw id → the decoy is opened, its
/// `ATTACH-OLD-USER` shows, and the refusal line is missing → FAILED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_refuses_a_path_like_id_without_reading_it() {
    let _serial = pty_guard();
    let scratch = TempDir::new("attach-bad-id");
    // `<scratch>/decoy.jsonl`, which is where `attach ../decoy` would resolve to.
    let _decoy = attach_session(scratch.path(), "decoy", 0).await;
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    let mut pty = Pty::spawn(
        &[
            "attach",
            "../decoy",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_raw(
            "must be 1-128 characters of [A-Za-z0-9._-]",
            wait_positive()
        ),
        "the one-line id refusal is not on the terminal:\n{}",
        pty.screen()
    );
    assert!(
        !String::from_utf8_lossy(&pty.raw()).contains("ATTACH-OLD-USER"),
        "the file outside the session directory was read:\n{}",
        pty.screen()
    );
    assert_eq!(
        {
            assert!(
                pty.wait_for_exit(wait_positive()).is_some(),
                "the refusal did not end the process:\n{}",
                pty.screen()
            );
            // `wait_for_exit` reports the shell wrapper's status; the app's own code is the
            // `EXITED:<n>` line it printed.
            pty.app_exit_code()
        },
        Some(2),
        "the refusal exits non-zero:\n{}",
        pty.screen()
    );
}
/// stays up, the journal's bytes are identical, no file appears in the session directory and
/// no control-port socket is created. attach has no input path at all, so this holds by
/// construction; the test is what keeps it so.
///
/// Mutation: route any of these keys anywhere (a command, a write, a control-port connection)
/// → the byte, run/ or directory assertion FAILED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_swallows_every_other_key() {
    let _serial = pty_guard();
    let dir = TempDir::new("attach-keys");
    let home = TempDir::new("attach-keys-home");
    // Held for the whole test: the writer lock stays taken, so a reader that tried to lock
    // would block instead of showing the transcript.
    let _journal = attach_session(dir.path(), "keys-me", 40).await;
    let path = dir.path().join("keys-me.jsonl");

    let mut pty = Pty::spawn_with_env(
        &[
            "attach",
            "keys-me",
            "--session-dir",
            dir.path().to_str().unwrap(),
        ],
        &[("RADIAN_HOME", home.path().to_str().unwrap())],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("ATTACH-FILLER-39", wait_positive()),
        "the committed history is not on screen:\n{}",
        pty.screen()
    );
    let before_bytes = std::fs::metadata(&path).unwrap().len();
    let before_run = run_entries(home.path());
    let before_sessions = dir_entries(dir.path());

    // Letters, punctuation, Enter, Tab, ctrl-f/ctrl-p, an arrow sequence and a bare Escape
    // sequence's prefix are all neither scroll nor quit keys. Esc and Ctrl-C are the quit keys
    // and are not sent.
    for key in [
        b"x".as_slice(),
        b"/".as_slice(),
        b"1".as_slice(),
        b"!".as_slice(),
        b"\r".as_slice(),
        b"\t".as_slice(),
        b"\x06".as_slice(),
        b"\x10".as_slice(),
        b"\x1b[A".as_slice(),
    ] {
        pty.send(key);
    }
    // PageUp is a scroll key: it moves the view and nothing else. Its effect on the footer is
    // also the **synchronization** — keys are handled in the order they arrive, so the footer
    // showing `paused` proves every key sent above it has been processed by now.
    pty.send(b"\x1b[5~");
    assert!(
        pty.wait_for_screen("paused", wait_positive()),
        "page up did not pause the follow:\n{}",
        pty.screen()
    );

    // So the assertions below are about keys the app has already handled.
    assert!(
        pty.wait_for_screen("attach · keys-me · read-only", wait_positive()),
        "the view stopped drawing:\n{}",
        pty.screen()
    );
    assert!(
        pty.child.try_wait().unwrap().is_none(),
        "attach must not quit on a key it ignores"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        before_bytes,
        "a key press changed the journal"
    );
    assert_eq!(
        run_entries(home.path()),
        before_run,
        "a key press opened a control-port connection"
    );
    assert_eq!(
        dir_entries(dir.path()),
        before_sessions,
        "a key press created a file"
    );

    pty.send(b"q");
    assert_eq!(
        pty.wait_for_exit(wait_positive()),
        Some(0),
        "q must quit the view"
    );
}

// ---------- ADR 0043: `radian attach` without an id (the workspace picker) ----------

/// Without an id, `attach` opens this workspace's picker — the `--pick` rows, with no "new
/// session" row — and `Enter` attaches read-only to the choice: the picked session's committed
/// transcript appears, the keys the view ignores reach nothing, and neither journal changes.
///
/// Mutation: open the picker with its "new session" row (leave `without_new` off the built
/// picker) → the `n new` hint is drawn and `n` leaves instead of filtering → FAILED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_without_an_id_picks_a_session_and_stays_read_only() {
    let _serial = pty_guard();
    let scratch = TempDir::new("attach-pick");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    seed_session(&sessions, "beta", &workspace, "BETA-HISTORY").await;
    // Both transcripts are taller than the pty (see `fill_session`), so a scroll key has
    // somewhere to go and the tail marker of the session that was *not* attached to must never
    // appear.
    fill_session(&sessions, "alpha", 40).await;
    fill_session(&sessions, "beta", 40).await;
    let before = jsonl_files(&sessions);

    let mut pty = Pty::spawn_in(
        &["attach", "--session-dir", sessions.to_str().unwrap()],
        &[],
        &ws,
        24,
        100,
    );
    // Both of this workspace's sessions are listed: the picker, not a refusal.
    assert!(
        pty.wait_for_screen("alpha", wait_positive()),
        "the picker did not list alpha:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("beta", wait_positive()),
        "the picker did not list beta:\n{}",
        pty.screen()
    );
    // No "new session" row: attach creates nothing, so it offers neither the hint nor the key.
    assert!(
        !pty.screen().contains("n new"),
        "the attach picker offers a new session:\n{}",
        pty.screen()
    );
    // `n` is an ordinary filter character here: it narrows the list and the process stays up.
    // (With a new-session row it would leave instead — exit 0 with no attach.)
    pty.send(b"n");
    assert!(
        pty.wait_for_screen("filter: n", wait_positive()),
        "`n` did not filter:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("alpha")),
        "the filter did not narrow the list:\n{}",
        pty.screen()
    );
    assert!(
        pty.child.try_wait().unwrap().is_none(),
        "`n` must not start or leave anything:\n{}",
        pty.screen()
    );

    // Backspace clears the filter. The panel's top border names the **selected** row
    // (`--resume <id>`), so the second row is whichever id the hint names after one Down —
    // the list's own order (newest activity first) is not this test's business.
    pty.send(&[0x7f]);
    assert!(
        pty.wait_for_screen("alpha", wait_positive()),
        "backspace did not restore the list:\n{}",
        pty.screen()
    );
    let first = picker_hint(&pty.screen()).expect("the first row's --resume hint is drawn");
    pty.send(b"\x1b[B");
    assert!(
        pty.wait_until(wait_positive(), |screen| {
            picker_hint(screen).is_some_and(|id| id != first)
        }),
        "Down did not move to the second row:\n{}",
        pty.screen()
    );
    let second = picker_hint(&pty.screen()).expect("the second row's --resume hint is drawn");
    assert!(
        ["alpha", "beta"].contains(&second.as_str()),
        "the hint names a session that was not seeded: {second:?}\n{}",
        pty.screen()
    );
    // The row that was left behind, whose history and tail must not be on screen.
    let other = if first == "beta" {
        "BETA-HISTORY"
    } else {
        "ALPHA-HISTORY"
    };
    // Enter attaches to the choice: its transcript, read-only.
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen(&format!("attach · {second} · read-only"), wait_positive()),
        "Enter did not attach to the second row:\n{}",
        pty.screen()
    );
    // The transcript is the picked session's, scrolled to its tail (the `{id}-END` marker), and
    // the other session's is nowhere.
    assert!(
        pty.wait_for_screen(&format!("{second}-END"), wait_positive()),
        "the picked session's transcript is not on screen:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains(&format!("{first}-END")),
        "the attached view shows the other session:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains(other),
        "the attached view shows the other session's history:\n{}",
        pty.screen()
    );
    // Typing reaches nothing: every key is swallowed. The PageUp that follows is also the
    // synchronization — keys are handled in order, so `paused` proves they were all handled —
    // and it keeps the view alive.
    for key in [
        b"x".as_slice(),
        b"1".as_slice(),
        b"\r".as_slice(),
        b"\t".as_slice(),
        b"/".as_slice(),
        b"\x1b[A".as_slice(),
    ] {
        pty.send(key);
    }
    pty.send(b"\x1b[5~");
    assert!(
        pty.wait_for_screen("paused", wait_positive()),
        "the attached view stopped handling keys:\n{}",
        pty.screen()
    );
    assert!(
        pty.child.try_wait().unwrap().is_none(),
        "a key quit the attached view:\n{}",
        pty.screen()
    );
    assert_eq!(
        jsonl_files(&sessions),
        before,
        "the attached view wrote a session file"
    );

    // Esc leaves the view with exit 0; nothing was written on either path.
    pty.send(&[0x1b]);
    assert_eq!(
        pty.wait_for_exit(wait_positive()),
        Some(0),
        "Esc must quit the view:\n{}",
        pty.screen()
    );
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(
        jsonl_files(&sessions),
        before,
        "attach wrote a session file"
    );
}

/// `Esc` at the picker, before any choice, leaves with exit 0 and creates nothing: no session
/// file appears or changes (the picker is not the `--pick` screen, which may start a session).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_without_an_id_escapes_the_picker_without_writing() {
    let _serial = pty_guard();
    let scratch = TempDir::new("attach-pick-esc");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    seed_session(&sessions, "beta", &workspace, "BETA-HISTORY").await;
    let before = jsonl_files(&sessions);

    let mut pty = Pty::spawn_in(
        &["attach", "--session-dir", sessions.to_str().unwrap()],
        &[],
        &ws,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("beta", wait_positive()),
        "the picker did not list beta:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "Esc did not leave the picker:\n{}",
        pty.screen()
    );
    assert_eq!(
        pty.app_exit_code(),
        Some(0),
        "Esc at the picker is a clean quit:\n{}",
        pty.screen()
    );
    assert_eq!(
        jsonl_files(&sessions),
        before,
        "Esc at the picker wrote a session file"
    );
}

/// `--follow` is honoured on the picker path too: after the choice, a line the session's own
/// writer commits appears in the attached view within a second — the same bound the id form's
/// follow test pins.
///
/// Mutation: drop `follow` when the choice is made (attach without the flag) → the appended
/// line never arrives → FAILED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_without_an_id_follows_the_chosen_session() {
    let _serial = pty_guard();
    use agent_core::TranscriptJournal;
    let scratch = TempDir::new("attach-pick-follow");
    let sessions = scratch.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = std::fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    seed_session(&sessions, "alpha", &workspace, "ALPHA-HISTORY").await;
    // The writer stays open for the rest of the test: the line it commits after the attach is a
    // real append by another process (ADR 0040), exactly what `--follow` is for.
    let journal = fill_session(&sessions, "alpha", 0).await;

    let mut pty = Pty::spawn_in(
        &[
            "attach",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--follow",
        ],
        &[],
        &ws,
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("alpha", wait_positive()),
        "the picker did not list alpha:\n{}",
        pty.screen()
    );
    // Type-to-filter, then Enter: the one matching row is the choice.
    pty.send(b"alp");
    assert!(
        pty.wait_for_screen("filter: alp", wait_positive()),
        "the filter did not apply:\n{}",
        pty.screen()
    );
    pty.send(b"\r");
    assert!(
        pty.wait_for_screen("attach · alpha · read-only", wait_positive()),
        "Enter did not attach to alpha:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("ALPHA-HISTORY", wait_positive()),
        "the committed history is not on screen:\n{}",
        pty.screen()
    );

    journal
        .append(agent_core::JournalRecord::Message {
            index: 2,
            message: agent_core::Message::assistant("ALPHA-FOLLOWED"),
        })
        .await
        .unwrap();
    assert!(
        pty.wait_for_screen("ALPHA-FOLLOWED", WAIT_LATENCY_BOUND),
        "the appended message did not arrive within a second:\n{}",
        pty.screen()
    );

    pty.send(b"q");
    assert_eq!(
        pty.wait_for_exit(wait_positive()),
        Some(0),
        "q must quit the view"
    );
}

/// With no sessions, `attach` says so in one line and exits 0 **without entering the alternate
/// screen**; nothing at all is created, not even the session directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_attach_without_an_id_and_no_sessions_says_so_without_a_screen() {
    let _serial = pty_guard();
    let scratch = TempDir::new("attach-pick-none");
    // Never created: `attach` must not create it either.
    let sessions = scratch.path().join("sessions");
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let mut pty = Pty::spawn_in(
        &["attach", "--session-dir", sessions.to_str().unwrap()],
        &[],
        &ws,
        24,
        100,
    );
    assert!(
        pty.wait_for_raw(
            "radian: no sessions for this workspace; nothing to attach",
            wait_positive()
        ),
        "the one-line notice is missing:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_exit(wait_positive()).is_some(),
        "attach did not exit:\n{}",
        pty.screen()
    );
    assert_eq!(
        pty.app_exit_code(),
        Some(0),
        "nothing to attach is not a failure:\n{}",
        pty.screen()
    );
    assert!(
        !String::from_utf8_lossy(&pty.raw()).contains("\x1b[?1049h"),
        "an empty workspace entered the alternate screen:\n{}",
        pty.screen()
    );
    assert!(!sessions.exists(), "attach created the session directory");
}

// ---------- lost input: a resize must not eat a typed key ----------

/// A printable key typed into an empty composer is text — including while a resize is being
/// coalesced. The coalescer used to read one non-resize event to find the end of a resize
/// burst and throw it away, so the first key after a resize was silently dropped.
#[test]
fn pty_a_resize_does_not_eat_a_typed_key() {
    let _serial = pty_guard();
    let dir = TempDir::new("resize-keeps-keys");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "resize-keys",
            "--offline-catalog",
        ],
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    // The resize lands while the input thread is idle and the text follows in one write, so
    // crossterm's read buffers the tail of it: that buffered key was the one the coalescer ate.
    pty.resize(31, 110);
    pty.send(b"abcdef");
    assert!(
        pty.wait_for_screen("abcdef", wait_positive()),
        "a key was dropped after the resize:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// Once the driver stopped, a submit cannot start a turn: every submit says the session is
/// dead and how to resume it, keeps the text, and reaches no model. The footer says `ended`,
/// never `idle` (nothing can start).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_submit_after_the_driver_ended_says_the_session_is_dead() {
    let _serial = pty_guard();
    let dir = TempDir::new("ended-submit");
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
            "ended-submit",
            "--panic-in-driver",
        ],
        40,
        110,
    );
    assert!(
        pty.wait_for_screen("the session ended: driver panicked", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello there\r");
    assert!(
        pty.wait_for_screen("message was not sent", wait_positive()),
        "the submit was silently swallowed:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("radian --resume ended-submit", wait_positive()),
        "the notice does not say how to continue:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("hello there"),
        "the message must stay in the composer:\n{}",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("idle"),
        "the footer must not claim a live turn:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("ended"),
        "the footer does not say the session ended:\n{}",
        pty.screen()
    );
    // Every submit says so again.
    pty.send(b"\r");
    assert!(
        pty.wait_until(wait_positive(), |screen| screen
            .matches("message was not sent")
            .count()
            >= 2),
        "the second submit was silent:\n{}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "no request may reach the model: {:?}",
        server.requests()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(1));
}

/// ADR 0008 §2: a fresh install with no config and no `--model` still starts the TUI. The
/// footer shows `no model`, the input box points at `/model`, a submit sends nothing (the
/// text stays) and reaches no model; picking one with `/model` writes the two keys to the
/// freshly created config file, and the next message reaches the mock model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_first_launch_starts_with_no_model_and_writes_back_the_first_pick() {
    let _serial = pty_guard();
    let dir = TempDir::new("first-launch");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let sessions = dir.path().join("sessions");
    let server = MockServer::start(vec![text_reply("FIRST-LAUNCH-OK")]).await;
    let mut pty = Pty::spawn_bare(
        &[
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "first-launch",
            "--offline-catalog",
        ],
        &[
            ("RADIAN_HOME", home.to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-first-launch-test"),
            ("RADIAN_BASE_URL", &server.base_url),
        ],
        30,
        110,
    );
    // The TUI starts instead of exiting: the footer and the input box say no model is set.
    assert!(
        pty.wait_for_screen("no model", wait_positive()),
        "the screen does not say no model is set:\n{}",
        pty.screen()
    );
    // A submit sends nothing, keeps the text, and reaches no model.
    pty.send(b"hello there\r");
    assert!(
        pty.wait_for_screen("no model is set", wait_positive()),
        "the submit was silently swallowed:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("hello there"),
        "the message must stay in the composer:\n{}",
        pty.screen()
    );
    assert!(
        server.requests().is_empty(),
        "no request may reach the model: {:?}",
        server.requests()
    );
    // Picking a model writes the two keys to the freshly created default config.
    // The blocked submit kept the draft, so clear the line first (Ctrl-U).
    pty.send(&[0x15]);
    pty.send(b"/model openai-compatible/mock\r\r");
    assert!(
        pty.wait_for_screen("next turn: openai-compatible/mock", wait_positive()),
        "the switch was not queued:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("model saved to", wait_positive()),
        "the write-back notice is missing:\n{}",
        pty.screen()
    );
    let config_path = home.join("config/config.toml");
    assert!(config_path.is_file(), "the default config was created");
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        text.contains("provider = \"openai-compatible\"") && text.contains("model = \"mock\""),
        "the two keys were not written back:\n{text}"
    );
    // The next message reaches the mock model.
    pty.send(b"hello again\r");
    assert!(
        pty.wait_for_screen("FIRST-LAUNCH-OK", wait_positive()),
        "the reply did not arrive:\n{}",
        pty.screen()
    );
    assert_eq!(server.requests().len(), 1, "one request after the pick");
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// ADR 0008 §2 + ADR 0026 D5: a session that started with a configured model never writes
/// the config on `/model` — the switch only advances the journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_a_model_switch_never_writes_when_a_model_was_configured() {
    let _serial = pty_guard();
    let dir = TempDir::new("model-no-write");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let sessions = dir.path().join("sessions");
    let server = MockServer::start(vec![text_reply("SWITCH-OK")]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m1\"\nbase_url = \"{}\"\n",
        server.base_url,
    );
    let mut pty = Pty::spawn_with_config(
        &[
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "model-no-write",
            "--offline-catalog",
        ],
        &[
            ("RADIAN_HOME", home.to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-no-write-test"),
            ("RADIAN_BASE_URL", &server.base_url),
        ],
        &config,
        30,
        110,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    let config_path = home.join("config/config.toml");
    let before = std::fs::read(&config_path).unwrap();
    pty.send(b"/model openai-compatible/m2\r\r");
    assert!(
        pty.wait_for_screen("next turn: openai-compatible/m2", wait_positive()),
        "the switch was not queued:\n{}",
        pty.screen()
    );
    // The write-back notice must never appear for a session that had a model.
    assert!(
        pty.screen_absent("model saved to"),
        "a configured session must not write the config:\n{}",
        pty.screen()
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "not one byte of the config may change"
    );
    pty.send(b"hi\r");
    assert!(
        pty.wait_for_screen("SWITCH-OK", wait_positive()),
        "the reply did not arrive:\n{}",
        pty.screen()
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "a later turn must not write either"
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "{}", pty.screen());
}

/// The inspector panel: `/jobs`, `/tools`, `/mcp` and `/hooks` each open their page, and
/// `/panel` returns to the last page. Esc closes to the composer.
#[test]
fn pty_inspector_four_commands_open_their_pages() {
    let _serial = pty_guard();
    let dir = TempDir::new("inspector-pages");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "inspector-pages",
            "--offline-catalog",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    // jobs: no background jobs yet.
    pty.send(b"/jobs\r");
    assert!(
        pty.wait_for_screen("inspector", wait_positive()),
        "the jobs page did not open:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("no background jobs", wait_positive()),
        "the jobs page shows no list:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("inspector")),
        "Esc did not close the panel:\n{}",
        pty.screen()
    );
    // tools: the built-ins are listed, the old names are not.
    pty.send(b"/tools\r");
    assert!(
        pty.wait_for_screen("inspector", wait_positive()),
        "the tools page did not open:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("read", wait_positive()),
        "the tools page shows no tools:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen_absent("job_output"),
        "an old name leaked into the tools page:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("inspector")),
        "Esc did not close the panel:\n{}",
        pty.screen()
    );
    // mcp: no servers configured.
    pty.send(b"/mcp\r");
    assert!(
        pty.wait_for_screen("no MCP servers", wait_positive()),
        "the mcp page did not open:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("inspector")),
        "Esc did not close the panel:\n{}",
        pty.screen()
    );
    // hooks: none configured.
    pty.send(b"/hooks\r");
    assert!(
        pty.wait_for_screen("no hooks configured", wait_positive()),
        "the hooks page did not open:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("inspector")),
        "Esc did not close the panel:\n{}",
        pty.screen()
    );
    // `/panel` returns to the last page (hooks), not jobs.
    pty.send(b"/panel\r");
    assert!(
        pty.wait_for_screen("no hooks configured", wait_positive()),
        "/panel did not return to the last page:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("inspector")),
        "Esc did not close the panel:\n{}",
        pty.screen()
    );
    // The composer kept the keys: typing after close types, it does not reopen.
    pty.send(b"hi");
    assert!(
        pty.wait_for_screen("hi", wait_positive()),
        "the composer did not keep the keys:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// The inspector opens while a turn runs and does not disturb it: the running state stays
/// visible under the panel, and the turn still settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_inspector_opens_during_a_running_turn() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let config = format!(
        "schema_version = 1\n[model]\nprovider = \"openai-compatible\"\nmodel = \"m\"\n\
         base_url = \"{}\"\n",
        server.base_url
    );
    let mut pty = Pty::spawn_with_config(&[], &[("OPENAI_API_KEY", "sk-test")], &config, 24, 100);
    assert!(
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"hello\r");
    assert!(
        pty.wait_for_screen("Working", wait_positive()),
        "the turn did not start:\n{}",
        pty.screen()
    );
    pty.send(b"/tools\r");
    assert!(
        pty.wait_for_screen("inspector", wait_positive()),
        "the panel did not open mid-turn:\n{}",
        pty.screen()
    );
    // The running state is still there: the panel did not swallow the turn.
    assert!(
        pty.screen().contains("Working"),
        "opening mid-turn hid the running state:\n{}",
        pty.screen()
    );
    pty.send(&[0x1b]);
    assert!(
        pty.wait_until(wait_positive(), |screen| !screen.contains("inspector")),
        "Esc did not close the panel:\n{}",
        pty.screen()
    );
    assert!(
        pty.screen().contains("Working"),
        "closing mid-turn stopped the turn:\n{}",
        pty.screen()
    );
    pty.quit();
}

/// The open panel is idle-zero-redraw like an idle screen (ADR 0023 U2, extended to the
/// panel layer): with no events, the frame count over an idle window must not move.
#[test]
fn pty_an_open_panel_is_not_redrawn() {
    let _serial = pty_guard();
    let dir = TempDir::new("inspector-idle");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "inspector-idle",
            "--offline-catalog",
        ],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("type a message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/tools\r");
    assert!(
        pty.wait_for_screen("inspector", wait_positive()),
        "{}",
        pty.screen()
    );
    let before = settled_frame_count(&mut pty);
    assert!(before >= 1, "the panel frame is drawn");
    let window = std::time::Instant::now();
    while window.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let after = frame_count(&pty);
    pty.quit();
    assert_eq!(
        after, before,
        "an open panel with no events must not be redrawn (ADR 0023 U2)"
    );
}

/// A text reply streamed slowly: `pause` between SSE pieces, so the turn stays running for a
/// few seconds while the test queues behind it.
fn slow_text_reply(text: &str, pause: Duration) -> Reply {
    let pieces = vec![
        format!(
            "data: {}\n\n",
            chunk(json!({"role": "assistant", "content": text}), None)
        )
        .into_bytes(),
        format!("data: {}\n\n", chunk(json!({}), Some("stop"))).into_bytes(),
        b"data: [DONE]\n\n".to_vec(),
    ];
    Reply::Sse {
        pieces,
        pause,
        headers: Vec::new(),
    }
}

/// `/queue <text>` while a turn runs: the running turn is untouched, and each queued message
/// opens its own turn in order when it ends (ADR 0011 L4 FIFO). The footer's `queued` item
/// shows the waiting count (2, then 1) and hides when drained.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_queue_during_a_running_turn_runs_queued_turns_in_order() {
    let _serial = pty_guard();
    let pause = Duration::from_secs(2);
    let server = MockServer::start(vec![
        slow_text_reply("FIRST-REPLY-OK", pause),
        slow_text_reply("SECOND-REPLY-OK", pause),
        slow_text_reply("THIRD-REPLY-OK", pause),
    ])
    .await;
    let dir = TempDir::new("queue");
    // The real defaults: the `deck` footer carries `queued` (the suite's `classic` does not).
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "queue",
        ],
        &[],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"start\r");
    // Wait until the request reached the mock: the turn is running now.
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.requests().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !server.requests().is_empty(),
        "the turn never started:\n{}",
        pty.screen()
    );
    pty.send(b"/queue alpha\r");
    pty.send(b"/queue beta\r");
    assert!(
        pty.wait_for_screen("2 queued", wait_positive()),
        "the queue count did not reach 2:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("FIRST-REPLY-OK", wait_positive()),
        "the first turn did not answer:\n{}",
        pty.screen()
    );
    // Alpha's turn admitted it: one waits while it streams.
    assert!(
        pty.wait_for_screen("1 queued", wait_positive()),
        "the count did not drop to 1:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("SECOND-REPLY-OK", wait_positive()),
        "alpha's turn did not answer:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("THIRD-REPLY-OK", wait_positive()),
        "beta's turn did not answer:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_until_gone("queued", wait_positive()),
        "the item did not hide after the drain:\n{}",
        pty.screen()
    );

    // Three turns, in order: the running turn never saw the queued text, and each queued
    // message opened its own turn carrying its own text.
    let bodies = server.requests();
    assert_eq!(bodies.len(), 3, "three turns ran");
    assert!(
        last_user_text(&server, 0).contains("start")
            && !last_user_text(&server, 0).contains("alpha"),
        "the running turn is untouched: {:?}",
        bodies[0].json()
    );
    assert!(
        last_user_text(&server, 1).contains("alpha"),
        "alpha opened the second turn: {:?}",
        bodies[1].json()
    );
    assert!(
        last_user_text(&server, 2).contains("beta"),
        "beta opened the third turn: {:?}",
        bodies[2].json()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `/queue <text>` while idle opens a turn at once, carrying the queued text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_queue_while_idle_starts_a_turn_at_once() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![text_reply("IDLE-QUEUE-REPLY-OK")]).await;
    let dir = TempDir::new("queue-idle");
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "queue-idle",
        ],
        &[],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"/queue do it now\r");
    assert!(
        pty.wait_for_screen("IDLE-QUEUE-REPLY-OK", wait_positive()),
        "the queued turn did not answer:\n{}",
        pty.screen()
    );
    let bodies = server.requests();
    assert_eq!(bodies.len(), 1, "one turn ran");
    assert!(
        last_user_text(&server, 0).contains("do it now"),
        "the turn carries the queued text: {:?}",
        bodies[0].json()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `/copy` copies the last committed assistant message through the clipboard backend, and
/// `/copy code` copies only its last fenced block: the loop's honest `copied N chars`
/// notice names what was copied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_copy_copies_the_last_message_and_its_code_block() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        text_reply("COPY-OK"),
        text_reply("```rust\nlet x = 1;\n```"),
    ])
    .await;
    let dir = TempDir::new("copy");
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "copy",
        ],
        &[],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"first\r");
    assert!(
        pty.wait_for_screen("COPY-OK", wait_positive()),
        "the reply did not arrive:\n{}",
        pty.screen()
    );
    pty.send(b"/copy\r");
    assert!(
        pty.wait_for_screen("copied 7 chars", wait_positive()),
        "the copy notice did not arrive:\n{}",
        pty.screen()
    );
    pty.send(b"second\r");
    assert!(
        pty.wait_for_screen("let x = 1;", wait_positive()),
        "the code reply did not arrive:\n{}",
        pty.screen()
    );
    pty.send(b"/copy code\r");
    assert!(
        pty.wait_for_screen("copied 10 chars", wait_positive()),
        "the code copy notice did not arrive:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// BELs in the raw pty bytes. The bell tests set `clipboard = "off"`, so no OSC 52 sequence
/// (which also ends in `\x07`) can confound the count: every BEL here is a bell ring.
fn bel_count(pty: &Pty) -> usize {
    String::from_utf8_lossy(&pty.raw()).matches('\x07').count()
}

/// `tui.bell = "attention"`: an approval prompt rings exactly once — no second ring for the
/// same prompt, and no ring for anything before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bell_attention_rings_once_for_an_approval() {
    let _serial = pty_guard();
    let dir = TempDir::new("bell-approval");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"approved"}"#,
        ),
        text_reply("WROTE-IT"),
    ])
    .await;
    let config =
        "schema_version = 1\n[tui]\nbell = \"attention\"\nclipboard = \"off\"\n".to_string();
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "bell-approval",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        &[],
        &config,
        30,
        140,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"please write\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "no approval prompt:\n{}",
        pty.screen()
    );
    assert_eq!(
        bel_count(&pty),
        1,
        "one prompt rings exactly once:\n{}",
        pty.screen()
    );
    // Answering and quitting rings nothing more (a cancel needs no ring).
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("WROTE-IT", wait_positive()),
        "the approved call did not complete:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(
        bel_count(&pty),
        2,
        "the prompt and the completed turn each rang once, nothing more"
    );
}

/// `tui.bell = "off"`: the same approval prompt stays silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bell_off_stays_silent() {
    let _serial = pty_guard();
    let dir = TempDir::new("bell-off");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"approved"}"#,
        ),
        text_reply("WROTE-IT"),
    ])
    .await;
    let config = "schema_version = 1\n[tui]\nbell = \"off\"\nclipboard = \"off\"\n".to_string();
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "bell-off",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "always",
        ],
        &[],
        &config,
        30,
        140,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"please write\r");
    assert!(
        pty.wait_for_screen("approve tool call?", wait_positive()),
        "no approval prompt:\n{}",
        pty.screen()
    );
    // The prompt is the positive sync; a ring would have fired with it. One beat for
    // scheduling slop, then silence.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(bel_count(&pty), 0, "off rings nothing");
    pty.send(b"y");
    assert!(
        pty.wait_for_screen("WROTE-IT", wait_positive()),
        "the approved call did not complete:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
    assert_eq!(bel_count(&pty), 0, "the turn end rang nothing either");
}

/// `tui.bell = "all"`: a background job's settle rings — on top of the turn-end ring, so
/// at least two BELs have fired once the settle's followup turn answers. The job sleeps
/// first, so its settle lands in a later frame than the turn end: the two rings can never
/// merge into one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_bell_all_rings_for_a_background_job_settle() {
    let _serial = pty_guard();
    let dir = TempDir::new("bell-job");
    let ws = dir.path().join("ws");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "shell",
            r#"{"command":"sleep 5","background":true}"#,
        ),
        text_reply("TURN1-OK"),
        text_reply("JOB-DONE-OK"),
    ])
    .await;
    let config = "schema_version = 1\n[tui]\nbell = \"all\"\nclipboard = \"off\"\n".to_string();
    let mut pty = Pty::spawn_with_config(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "bell-job",
            "--workspace",
            ws.to_str().unwrap(),
            "--approval",
            "auto",
        ],
        &[],
        &config,
        30,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"run it in the background\r");
    assert!(
        pty.wait_for_screen("TURN1-OK", wait_positive()),
        "the first turn did not answer:\n{}",
        pty.screen()
    );
    // The first turn ended (one ring) while the job still sleeps; the settle follows in a
    // later frame (the second ring), then its followup turn answers.
    assert!(
        pty.wait_for_screen("JOB-DONE-OK", wait_positive()),
        "the settle never cycled through a turn:\n{}",
        pty.screen()
    );
    assert!(
        bel_count(&pty) >= 2,
        "the turn end and the settle both rang:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}
/// message matches the first one's, and the transcript gains a second user message without
/// rewriting anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_retry_after_a_model_error_resends_the_same_text() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        Reply::status(
            400,
            r#"{"error": {"message": "bad request", "type": "invalid_request_error"}}"#,
        ),
        text_reply("RETRY-REPLY-OK"),
    ])
    .await;
    let dir = TempDir::new("retry");
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "retry",
        ],
        &[],
        24,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "screen:\n{}",
        pty.screen()
    );
    pty.send(b"say it again\r");
    assert!(
        pty.wait_for_screen("turn failed", wait_positive()),
        "the failure was not shown:\n{}",
        pty.screen()
    );
    assert!(
        pty.wait_for_screen("/retry to resend", wait_positive()),
        "the retry hint was not shown:\n{}",
        pty.screen()
    );
    pty.send(b"/retry\r");
    assert!(
        pty.wait_for_screen("RETRY-REPLY-OK", wait_positive()),
        "the retried turn did not answer:\n{}",
        pty.screen()
    );
    // The same text twice: the retry resent the failed turn's input verbatim.
    assert_eq!(
        last_user_text(&server, 0),
        last_user_text(&server, 1),
        "both requests carry the same user text"
    );
    assert!(
        last_user_text(&server, 0).contains("say it again"),
        "the text is the typed message"
    );
    // The transcript gained a second user message; nothing was rewritten.
    assert!(
        pty.screen().matches("say it again").count() >= 2,
        "two user messages on screen:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0));
}

/// `/loop --times 5` re-sends the same prompt every turn: the second request carries the
/// same user text, the footer counts `loop 1/5` while the hanging second turn runs, Esc
/// pauses the loop, and `/help` lists `/loop`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn pty_loop_resends_until_times_and_footer_shows_it() {
    let _serial = pty_guard();
    let server = MockServer::start(vec![
        text_reply("LOOP-REPLY-ONE"),
        Reply::SseThenHang { pieces: Vec::new() },
    ])
    .await;
    let dir = TempDir::new("loop");
    // The shipped default footer (the `deck` preset carries the `loop` item): the suite's
    // `classic` default has no `loop` row, so `spawn_bare` writes no config file. Fifty
    // rows fit the whole `/help` list, so the `/loop` line cannot scroll off.
    let mut pty = Pty::spawn_bare(
        &[
            "--model",
            "mock",
            "--base-url",
            &server.base_url,
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "loop",
        ],
        &[],
        50,
        100,
    );
    assert!(
        pty.wait_for_screen("message", wait_positive()),
        "{}",
        pty.screen()
    );
    pty.send(b"/loop --times 5 loop-prompt-xyzzy\r");
    assert!(
        pty.wait_for_screen("LOOP-REPLY-ONE", wait_positive()),
        "the first turn did not answer:\n{}",
        pty.screen()
    );
    // The second turn hangs on purpose: the footer stably shows the iteration count.
    assert!(
        pty.wait_for_screen("loop 1/5", wait_positive()),
        "the footer did not count the loop iteration:\n{}",
        pty.screen()
    );
    // Both turns reached the model with the same prompt.
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.requests().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.requests().len(), 2, "two turns ran");
    assert_eq!(
        last_user_text(&server, 0),
        last_user_text(&server, 1),
        "both requests carry the loop prompt"
    );
    assert!(
        last_user_text(&server, 0).contains("loop-prompt-xyzzy"),
        "the text is the typed prompt"
    );
    // Esc pauses the hanging loop turn (the mode stays on, the prompt is dropped).
    pty.send(&[0x1b]);
    assert!(
        pty.wait_for_screen("loop paused", wait_positive()),
        "Esc did not pause the loop:\n{}",
        pty.screen()
    );
    pty.send(b"/help\r");
    assert!(
        pty.wait_for_screen("/loop", wait_positive()),
        "/help does not list /loop:\n{}",
        pty.screen()
    );
    pty.quit();
    assert_eq!(pty.app_exit_code(), Some(0), "clean exit code");
}

//! Rendering baseline for the transcript view (ADR 0023 Step 0).
//!
//! Since 2026-10-13 the transcript is virtualized (dogfooding D9): a frame wraps only the
//! entries on screen and sums cached line counts for the rest. The first frame measures every
//! entry once; the steady-state frames are what a budget of 16.67 ms/frame (60 fps) applies
//! to.
//!
//! Most measurements drive the **public API** only ([`App`], [`render::draw`], the ratatui
//! `TestBackend`), so the same test runs after an internal refactor of `app.rs`/`main.rs` as
//! long as those names hold. Two things `TestBackend` cannot see go through a real pty
//! (`libc::openpty`): the idle redraw loop (it lives in `main.rs`) and `--resume` startup.
//!
//! `TestBackend` measures **our** render cost only; it does not include terminal I/O latency,
//! the terminal's own parsing, or the compositor. The pty numbers include a real tty write and
//! a 1 ms detection poll, so they are quantized upward by up to ~1 ms.
//!
//! Measurements, not assertions: every test is `#[ignore]`d, so `cargo test` is unchanged.
//! Run:
//!
//! ```sh
//! cargo test --release -p radian-tui --test perf_render -- --ignored --nocapture --test-threads=1
//! ```

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use radian_tui::app::{App, Entry, EntryKind, Input, TurnState};
use radian_tui::projection::{SessionProjection, StreamKind};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// The long-session generator from the `pi/longsess` slice (Phase 5), shared via `#[path]` so
// the `--resume` numbers here use the same file as the session/runtime measurements.
#[path = "../../radian-session/tests/common/long_session.rs"]
mod long_session;

/// The standard measurement viewport.
const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;

/// How many long (2 KiB) tool results appear per 20 entries.
const LONG_TOOL_RESULTS_PER_20: usize = 3;
const LONG_TOOL_RESULT_BYTES: usize = 2 * 1024;

fn entries(n: usize) -> Vec<Entry> {
    let long_body =
        "the build failed while linking the final artifact\n".repeat(LONG_TOOL_RESULT_BYTES / 51);
    let chinese = "这是一段中文说明，用来测试在终端宽度下的换行与显示宽度计算是否正确。";
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let entry = match i % 20 {
            0 => Entry::new(
                EntryKind::User,
                format!("question {i}: explain the failure"),
            ),
            1 => Entry::new(
                EntryKind::Assistant,
                format!("Here is explanation {i} with a paragraph of ordinary English text."),
            ),
            2..=3 => Entry::new(EntryKind::Reasoning, format!("considering option {i} ...")),
            4..=6 => Entry::new(
                EntryKind::ToolCall,
                format!("read {{\"path\":\"src/module_{i}.rs\"}}"),
            ),
            7..=9 => Entry::new(
                EntryKind::ToolResult,
                format!("{long_body}ERROR: linker failed {i}"),
            ),
            10 => Entry::new(EntryKind::Assistant, chinese.repeat(2)),
            11..=12 => Entry::new(EntryKind::User, format!("next step {i}")),
            13..=19 => Entry::new(
                EntryKind::ToolResult,
                format!("ok: processed {i}\nsome ordinary output line for entry {i}"),
            ),
            _ => unreachable!(),
        };
        out.push(entry);
    }
    // `LONG_TOOL_RESULTS_PER_20` is documentation for the mix above (3 of 20 are long).
    debug_assert_eq!(
        (0..20).filter(|i| (7..=9).contains(&(i % 20))).count(),
        LONG_TOOL_RESULTS_PER_20
    );
    out
}

fn new_app(n: usize) -> App {
    let mut app = App::new("mock", "perf");
    app.entries = entries(n);
    app
}

fn test_terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap()
}

/// One `draw`, timed.
fn draw_once(terminal: &mut Terminal<TestBackend>, app: &mut App) -> Duration {
    let start = Instant::now();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    start.elapsed()
}

/// P50, P95 and max of `samples` timed draws, after one warm-up frame (layout + wrap caches).
fn measure_draws(
    app: &mut App,
    terminal: &mut Terminal<TestBackend>,
    samples: usize,
) -> (Duration, Duration, Duration) {
    let _ = draw_once(terminal, app);
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        times.push(draw_once(terminal, app));
    }
    percentiles(times)
}

/// P50, P95 and max. Sorts in place.
fn percentiles(mut v: Vec<Duration>) -> (Duration, Duration, Duration) {
    assert!(!v.is_empty());
    v.sort();
    let p50 = v[v.len() / 2];
    let p95 = v[(v.len() * 95 / 100).min(v.len() - 1)];
    (p50, p95, *v.last().unwrap())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Steady-state frame time at the standard viewport, per entry count.
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_render_frame_time() {
    let budget = Duration::from_millis(16);
    let mut first_over: Option<usize> = None;
    for n in [100usize, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000] {
        let samples = 20;
        let mut app = new_app(n);
        let mut terminal = test_terminal();
        let (median, p95, max) = measure_draws(&mut app, &mut terminal, samples);
        let over = median > budget || p95 > budget;
        println!(
            "entries {n:>6} @120x40: median {:.2} ms  p95 {:.2} ms  max {:.2} ms  {}",
            ms(median),
            ms(p95),
            ms(max),
            if over { "  > 16 ms" } else { "" }
        );
        if over && first_over.is_none() {
            first_over = Some(n);
        }
    }
    match first_over {
        Some(n) => println!("first entry count over the 16 ms budget: {n}"),
        None => println!("no measured entry count exceeded the 16 ms budget"),
    }
}

/// What a width change costs: the first frame at the new width re-wraps every entry whose wrap
/// cache is keyed by the old width, so it is a whole-transcript pass, unlike a steady frame.
/// The width alternates 120 <-> 80, so both directions are measured and the load spikes
/// average out over the samples.
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_render_resize() {
    for n in [1_000usize, 10_000, 50_000] {
        let mut app = new_app(n);
        let mut terminal = test_terminal();
        let (steady120, _, _) = measure_draws(&mut app, &mut terminal, 5);
        let mut first80 = Vec::with_capacity(5);
        let mut first120 = Vec::with_capacity(5);
        let mut steady80 = Vec::with_capacity(15);
        for _ in 0..5 {
            terminal.backend_mut().resize(80, HEIGHT);
            first80.push(draw_once(&mut terminal, &mut app));
            for _ in 0..3 {
                steady80.push(draw_once(&mut terminal, &mut app));
            }
            terminal.backend_mut().resize(WIDTH, HEIGHT);
            first120.push(draw_once(&mut terminal, &mut app));
            for _ in 0..3 {
                let _ = draw_once(&mut terminal, &mut app);
            }
        }
        let (f80_50, f80_95, _) = percentiles(first80);
        let (f120_50, f120_95, _) = percentiles(first120);
        let (s80_50, _, s80_max) = percentiles(steady80);
        println!(
            "entries {n:>6}: steady@120 {:.2} ms | first @80 p50 {:.2} ms p95 {:.2} ms | first @120 p50 {:.2} ms p95 {:.2} ms | steady@80 {:.2} ms (max {:.2})",
            ms(steady120),
            ms(f80_50),
            ms(f80_95),
            ms(f120_50),
            ms(f120_95),
            ms(s80_50),
            ms(s80_max)
        );
    }
}

/// Scrolling a 50k-entry transcript: `PageUp` ×100 from the bottom, then `Ctrl-Home` to the
/// top. Each key is paired with the frame that shows its effect.
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_render_scroll_50k() {
    let mut app = new_app(50_000);
    let mut terminal = test_terminal();
    let _ = draw_once(&mut terminal, &mut app);
    assert!(app.is_following(), "a fresh app follows the bottom");

    let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
    let mut per_key = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        app.on_key(page_up);
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        per_key.push(start.elapsed());
    }
    let total: Duration = per_key.iter().sum();
    let (p50, p95, max) = percentiles(per_key);

    let start = Instant::now();
    app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let ctrl_home = start.elapsed();
    assert_eq!(app.scroll, 0, "Ctrl-Home must reach the top");
    assert!(!app.is_following(), "the top is not the bottom");
    let mut to_top = vec![ctrl_home];
    for _ in 0..4 {
        app.scroll_to_bottom();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        let start = Instant::now();
        app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        to_top.push(start.elapsed());
        assert_eq!(app.scroll, 0, "Ctrl-Home must reach the top");
    }
    let (home50, home95, home_max) = percentiles(to_top);

    println!(
        "50k entries: PageUp x100 total {:.1} ms, per key p50 {:.2} ms p95 {:.2} ms max {:.2} ms",
        ms(total),
        ms(p50),
        ms(p95),
        ms(max)
    );
    println!(
        "50k entries: Ctrl-Home to top (key + frame) p50 {:.2} ms p95 {:.2} ms max {:.2} ms",
        ms(home50),
        ms(home95),
        ms(home_max)
    );
}

/// The event loop's streaming load: a text delta every millisecond for 5 s (5 000 deltas).
/// The run loop drains the event channel every 50 ms, so one frame sees ~50 deltas; that is
/// what this models (50 deltas then one frame, 100 rounds), on one focused app plus three
/// background apps streaming at the same rate. The harness is **not** paced: the scenario's
/// 5 000 deltas are applied as fast as the loop accepts them, so the wall time is shorter
/// than 5 s and the achieved delta rate is reported. Per-frame work is what the 1 ms rate
/// decides (50 deltas/frame), and it is unchanged by the pacing, so the frame and key times
/// are the measurement; running hotter can only make them conservative.
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_render_streaming() {
    const ROUNDS: usize = 100;
    const DELTAS_PER_ROUND: usize = 50;
    const DELTA: &str = "streaming token of ordinary length ";

    let mut apps: Vec<App> = (0..4)
        .map(|_| {
            let app = new_app(1_000);
            app.projection.set_turn(TurnState::Running);
            app
        })
        .collect();
    // Each app has its own projection now (ADR 0023 U2): the deltas accumulate there and the
    // snapshot becomes the live entry, which is what the loop does.
    let mut projections: Vec<SessionProjection> =
        apps.iter().map(|_| SessionProjection::new()).collect();
    let mut terminal = test_terminal();
    let _ = draw_once(&mut terminal, &mut apps[0]);

    let start = Instant::now();
    let mut frames = Vec::with_capacity(ROUNDS);
    let mut keys = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        for (app, projection) in apps.iter_mut().zip(projections.iter_mut()) {
            for _ in 0..DELTAS_PER_ROUND {
                projection.push_delta(StreamKind::Assistant, DELTA);
            }
            app.update(Input::Stream(projection.snapshot()));
        }
        frames.push(draw_once(&mut terminal, &mut apps[0]));
        let key = Instant::now();
        let _ = apps[0].on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        keys.push(key.elapsed());
    }
    let total = start.elapsed();

    let (frame50, frame95, frame_max) = percentiles(frames);
    let (key50, key95, key_max) = percentiles(keys);
    println!(
        "streaming (4 apps, {DELTAS_PER_ROUND} deltas/round, {ROUNDS} rounds in {:.2} s = {:.0} deltas/s):",
        total.as_secs_f64(),
        (ROUNDS * DELTAS_PER_ROUND) as f64 / total.as_secs_f64()
    );
    println!(
        "  frame p50 {:.2} ms  p95 {:.2} ms  max {:.2} ms",
        ms(frame50),
        ms(frame95),
        ms(frame_max)
    );
    println!(
        "  key apply p50 {:.3} ms  p95 {:.3} ms  max {:.3} ms",
        ms(key50),
        ms(key95),
        ms(key_max)
    );
}

/// Peak RSS after building a 50k-entry `App` (and drawing one frame). `ru_maxrss` is the
/// process's high-water mark, so the reading must come from a **fresh** process: every other
/// test in this binary builds a 50k `App` too, and an in-process reading would just report
/// their peak. The outer run re-executes this test binary for that one test.
#[cfg(unix)]
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_render_memory_50k() {
    if std::env::var_os("PERF_RENDER_MEMORY_CHILD").is_none() {
        let exe = std::env::current_exe().unwrap();
        let out = Command::new(exe)
            .env("PERF_RENDER_MEMORY_CHILD", "1")
            .args([
                "--exact",
                "perf_render_memory_50k",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "memory child failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with("50k entries:") {
                println!("{line}");
            }
        }
        return;
    }
    let base = max_rss_kib();
    let mut app = new_app(50_000);
    let built = max_rss_kib();
    let mut terminal = test_terminal();
    let _ = draw_once(&mut terminal, &mut app);
    let drawn = max_rss_kib();
    println!(
        "50k entries: RSS base {} MiB, after App {} MiB (+{} MiB), after first frame {} MiB (+{} MiB)",
        base / 1024,
        built / 1024,
        (built - base) / 1024,
        drawn / 1024,
        (drawn - built) / 1024
    );
    std::hint::black_box(&app);
}

/// The process's peak resident set size, in KiB (`getrusage(RUSAGE_SELF)`).
#[cfg(unix)]
fn max_rss_kib() -> u64 {
    // SAFETY: getrusage writes into a zeroed `rusage` and takes a valid enum constant.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage
    };
    // macOS reports bytes, Linux KiB.
    #[cfg(target_os = "linux")]
    {
        usage.ru_maxrss as u64
    }
    #[cfg(not(target_os = "linux"))]
    {
        usage.ru_maxrss as u64 / 1024
    }
}

// ---------------------------------------------------------------------------------------
// pty: the idle redraw loop and `--resume` startup, which `TestBackend` cannot see.
// ---------------------------------------------------------------------------------------

const BIN: &str = env!("CARGO_BIN_EXE_radian-tui");
const POLL: Duration = Duration::from_millis(1);
/// ratatui's "show cursor" write; the app calls `set_cursor_position` every frame while the
/// input box is editable, so it is a per-frame marker on the wire.
const FRAME_MARKER: &[u8] = b"[?25h";

struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-perf-render-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A child on a pty: `libc::openpty`, `std::process::Command` (posix_spawn), a reader thread.
/// The same harness as `perf_startup.rs`: `portable-pty`'s fork delayed the first byte by ~2 s
/// on this machine, which is harness cost, not the app's.
struct Pty {
    master: std::fs::File,
    bytes: Arc<Mutex<Vec<u8>>>,
    start: Instant,
    child: Child,
    _home: TempDir,
}
impl Drop for Pty {
    fn drop(&mut self) {
        // The binary is spawned directly (no `sh` wrapper), so killing the child is enough.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Pty {
    fn spawn(args: &[&str], rows: u16, cols: u16) -> Self {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        ws.ws_row = rows;
        ws.ws_col = cols;
        // SAFETY: openpty writes the two fds; the winsize pointer is valid for the call.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(ws),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: fresh fds from openpty; `OwnedFd` closes them.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

        let start = Instant::now();
        let mut cmd = Command::new(BIN);
        cmd.args(args);
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()));
        cmd.stdout(Stdio::from(slave.try_clone().unwrap()));
        cmd.stderr(Stdio::from(slave));
        cmd.env_clear();
        let home = TempDir::new("perf-home");
        for name in ["PATH", "TMPDIR", "LANG", "TERM"] {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("HOME", home.path());
        // A closed local port fails the catalog fetch fast (ADR 0018); no real network.
        cmd.env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json");
        let child = cmd.spawn().unwrap();

        let mut master = std::fs::File::from(master);
        let mut reader = master.try_clone().unwrap();
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
        let _ = master.flush();
        Self {
            master,
            bytes,
            start,
            child,
            _home: home,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    fn contains(&self, needle: &str) -> bool {
        contains_bytes(&self.bytes.lock().unwrap(), needle.as_bytes())
    }

    fn count(&self, needle: &[u8]) -> usize {
        count_bytes(&self.bytes.lock().unwrap(), needle)
    }

    /// Waits for `needle` and returns the time since `start`, or panics.
    fn wait_for(&self, needle: &str, start: Instant, secs: u64) -> Duration {
        let deadline = start + Duration::from_secs(secs);
        loop {
            if self.contains(needle) {
                return start.elapsed();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}"
            );
            std::thread::sleep(POLL);
        }
    }

    /// Waits until the pty has seen at least `n` frame markers.
    fn wait_for_frames(&self, n: usize, secs: u64) -> Duration {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if self.count(FRAME_MARKER) >= n {
                return self.start.elapsed();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {n} frames (saw {})",
                self.count(FRAME_MARKER)
            );
            std::thread::sleep(POLL);
        }
    }

    /// Ctrl-C, then a kill as a last resort: a measurement must not leave a process behind.
    fn quit(&mut self) {
        for _ in 0..4 {
            let _ = self.master.write_all(&[0x03]);
            let _ = self.master.flush();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(POLL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut at = 0;
    while let Some(pos) = haystack[at..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        count += 1;
        at += pos + needle.len();
    }
    count
}

static PTY_LOCK: Mutex<()> = Mutex::new(());
fn guard() -> std::sync::MutexGuard<'static, ()> {
    PTY_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The idle redraw (ADR 0023 U2). The loop blocks on its event channel, so an idle screen is
/// not redrawn at all; the Step 0 baseline (§7) measured ~19 frames/s under the old polling
/// loop. Measured as the bytes and frame markers on the tty over a 5 s window (the window is
/// observation time, not synchronization).
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_idle_redraw() {
    let _serial = guard();
    let dir = TempDir::new("idle");
    let mut pty = Pty::spawn(
        &[
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--session-id",
            "idle",
        ],
        24,
        100,
    );
    pty.wait_for("type a message", pty.start, 30);
    // The startup burst is over once the first frame is on the wire: the whole screen is drawn
    // once, and nothing else happens until an event arrives.
    pty.wait_for_frames(1, 30);
    let bytes_before = pty.snapshot().len();
    let frames_before = pty.count(FRAME_MARKER);

    let window = Instant::now();
    while window.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let bytes_after = pty.snapshot().len();
    let frames_after = pty.count(FRAME_MARKER);
    let elapsed = window.elapsed();
    pty.quit();

    let bytes = bytes_after - bytes_before;
    let frames = frames_after - frames_before;
    println!(
        "idle redraw over {:.2} s: {frames} frames ({:.1}/s), {bytes} bytes ({:.0} B/frame)",
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64(),
        bytes as f64 / frames.max(1) as f64
    );
}

/// A valid v1 session with `n` messages, using the shared long-session generator.
///
/// `radian-tui --resume`: process start to the first frame, then `Ctrl-Home` to the top. The
/// history is rendered before the run loop, so the first frame is the one that shows the
/// bottom (the `resumed session …` notice). The top of the generated transcript is the first
/// unit's tool call (`read({"path":"file.txt"})`), which is not on screen at the bottom.
#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_tui_resume() {
    let _serial = guard();
    // Warm the page cache / dyld once, so the first sample is not a cold run.
    {
        let dir = TempDir::new("resume-warmup");
        long_session::write_long_session(&dir.path().join("resume.jsonl"), "resume", 1_000, 0);
        let mut pty = Pty::spawn(
            &[
                "--model",
                "mock",
                "--base-url",
                "http://127.0.0.1:1/v1",
                "--session-dir",
                dir.path().to_str().unwrap(),
                "--resume",
                "resume",
            ],
            24,
            100,
        );
        pty.wait_for("resumed session", pty.start, 120);
        pty.quit();
    }
    for n in [1_000usize, 10_000, 50_000] {
        let dir = TempDir::new(&format!("resume-{n}"));
        let stats =
            long_session::write_long_session(&dir.path().join("resume.jsonl"), "resume", n, 0);
        let mut first = Vec::new();
        let mut home = Vec::new();
        for _ in 0..5 {
            let mut pty = Pty::spawn(
                &[
                    "--model",
                    "mock",
                    "--base-url",
                    "http://127.0.0.1:1/v1",
                    "--session-dir",
                    dir.path().to_str().unwrap(),
                    "--resume",
                    "resume",
                ],
                24,
                100,
            );
            first.push(pty.wait_for("resumed session", pty.start, 120));
            // Ctrl-Home is CSI 1;5 H (Home with the CONTROL modifier).
            let start = Instant::now();
            pty.send(b"\x1b[1;5H");
            home.push(pty.wait_for(r#"read({"path":"file.txt"})"#, start, 30));
            pty.quit();
        }
        let (first50, first95, first_max) = percentiles(first);
        let (home50, home95, home_max) = percentiles(home);
        println!(
            "tui --resume {n:>6} msgs ({} raw, {} live, {} MB) to first frame: p50 {:.0} ms  p95 {:.0} ms  max {:.0} ms",
            stats.raw_messages,
            stats.live_messages,
            stats.bytes / (1024 * 1024),
            ms(first50),
            ms(first95),
            ms(first_max)
        );
        println!(
            "tui --resume {n:>6} msgs Ctrl-Home to top: p50 {:.1} ms  p95 {:.1} ms  max {:.1} ms",
            ms(home50),
            ms(home95),
            ms(home_max)
        );
    }
}

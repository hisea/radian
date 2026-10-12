//! Phase 0b spike 2 的全屏 TUI 原型。**永远不要在自己的终端里直接运行**：它进备用屏、
//! 开 raw mode；一切验证都由 driver 经 pty 驱动（`--backend pty`），或用 TestBackend
//! 在内存里测（`--backend test`）。
//!
//! 模式：
//! - `stream`：每秒 `--tokens-per-sec`（默认 200）个 token 流入，逐帧重画并计时
//! - `idle`：无输入静置（驱动侧采样 CPU）
//! - `paste`：等待 bracketed paste，统计字节数与校验和
//! - `width`：渲染 CJK/emoji/全半角样例（驱动侧用 vt100 屏幕模型核对占宽）
//! - `resize`：全屏带边框布局，状态行显示 `size:WxH`（驱动侧改 pty 尺寸）
//!
//! 键：`q` 或 Ctrl-C 干净退出（恢复终端）；`p` 触发 panic（panic hook 里恢复终端）。

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stream,
    Idle,
    Paste,
    Width,
    Resize,
}

struct Args {
    mode: Mode,
    duration: Duration,
    stats: PathBuf,
    backend: BackendKind,
    tokens_per_sec: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Pty,
    Test,
}

fn parse_args() -> Args {
    let mut args = Args {
        mode: Mode::Idle,
        duration: Duration::from_secs(10),
        stats: PathBuf::from("stats.json"),
        backend: BackendKind::Pty,
        tokens_per_sec: 200,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => {
                args.mode = match it.next().as_deref() {
                    Some("stream") => Mode::Stream,
                    Some("idle") => Mode::Idle,
                    Some("paste") => Mode::Paste,
                    Some("width") => Mode::Width,
                    Some("resize") => Mode::Resize,
                    other => panic!("unknown mode {other:?}"),
                };
            }
            "--duration-secs" => {
                let s: u64 = it.next().unwrap().parse().unwrap();
                args.duration = Duration::from_secs(s);
            }
            "--stats" => args.stats = PathBuf::from(it.next().unwrap()),
            "--backend" => {
                args.backend = match it.next().as_deref() {
                    Some("pty") => BackendKind::Pty,
                    Some("test") => BackendKind::Test,
                    other => panic!("unknown backend {other:?}"),
                };
            }
            "--tokens-per-sec" => {
                args.tokens_per_sec = it.next().unwrap().parse().unwrap();
            }
            other => panic!("unknown arg {other:?}"),
        }
    }
    args
}

/// 终端状态守卫：进入时设置，Drop 或显式恢复时还原。panic hook 也调用 restore。
struct TtyGuard {
    active: bool,
}

impl TtyGuard {
    fn enter() -> Self {
        let mut g = Self { active: false };
        if let Err(e) = ratatui::crossterm::terminal::enable_raw_mode() {
            eprintln!("spike: enable_raw_mode failed: {e}");
            return g;
        }
        let mut out = io::stdout();
        let _ = execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            ratatui::crossterm::cursor::Hide
        );
        g.active = true;
        g
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut out = io::stdout();
        let _ = execute!(
            out,
            ratatui::crossterm::cursor::Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = out.flush();
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// 一次运行的状态与统计。
struct Run {
    lines: Vec<String>,
    status: String,
    frame_ms: Vec<f64>,
    tokens: u64,
    paste_events: u64,
    paste_bytes: usize,
    paste_sum: u64,
    stats_extra_sent: u64,
    quit: bool,
    panic: bool,
}

impl Run {
    fn new(status: String) -> Self {
        Self {
            lines: Vec::new(),
            status,
            frame_ms: Vec::new(),
            tokens: 0,
            paste_events: 0,
            paste_bytes: 0,
            paste_sum: 0,
            stats_extra_sent: 0,
            quit: false,
            panic: false,
        }
    }
}

const WORDS: &[&str] = &[
    "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "rust", "terminal", "stream",
    "frame", "paste", "width", "resize", "sandbox",
];

fn append_token(run: &mut Run, word: &str) {
    if let Some(last) = run.lines.last_mut()
        && last.chars().count() < 64
    {
        last.push(' ');
        last.push_str(word);
        return;
    }
    run.lines.push(word.to_string());
    if run.lines.len() > 400 {
        run.lines.remove(0);
    }
}

fn draw<B: Backend>(terminal: &mut Terminal<B>, run: &Run) -> f64 {
    let t = Instant::now();
    let lines: Vec<Line> = run.lines.iter().map(|l| Line::from(l.as_str())).collect();
    let status = run.status.clone();
    let tokens = run.tokens;
    terminal
        .draw(|f| {
            let area = f.area();
            let (body, status_area) = (
                ratatui::layout::Rect {
                    height: area.height.saturating_sub(1),
                    ..area
                },
                ratatui::layout::Rect {
                    y: area.height.saturating_sub(1),
                    height: 1,
                    ..area
                },
            );
            let para = Paragraph::new(lines).style(Style::default().fg(Color::Gray));
            f.render_widget(para, body);
            let bar = Line::from(vec![
                Span::styled(
                    format!(" {status} "),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(format!(" tokens:{tokens} frames:{} ", run.frame_ms.len())),
                Span::styled(
                    "q quit  p panic",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]);
            f.render_widget(Paragraph::new(bar), status_area);
        })
        .expect("draw");
    let _ = terminal.backend_mut().flush();
    t.elapsed().as_secs_f64() * 1000.0
}

fn width_samples() -> Vec<String> {
    vec![
        "S1:|中文abc|".to_string(),
        "S2:|日本語の幅|".to_string(),
        "S3:|🎉🚀emoji|".to_string(),
        "S4:|ＦＵＬＬｗｉｄｔｈ|".to_string(),
        "S5:|한국어|".to_string(),
        "S6:|ｱｲｳ半角|".to_string(),
        "S7:|e\u{0301}组合|".to_string(),
    ]
}

fn draw_width<B: Backend>(terminal: &mut Terminal<B>) -> f64 {
    let samples = width_samples();
    let t = Instant::now();
    terminal
        .draw(|f| {
            let area = f.area();
            let lines: Vec<Line> = samples.iter().map(|s| Line::from(s.as_str())).collect();
            f.render_widget(
                Paragraph::new(lines).style(Style::default().fg(Color::White)),
                ratatui::layout::Rect {
                    x: 1,
                    y: 1,
                    width: area.width.saturating_sub(2),
                    height: area.height.saturating_sub(3),
                },
            );
            let bar = Line::from(" width-shown q quit ");
            f.render_widget(
                Paragraph::new(bar).style(Style::default().fg(Color::Black).bg(Color::Yellow)),
                ratatui::layout::Rect {
                    y: area.height.saturating_sub(1),
                    height: 1,
                    ..area
                },
            );
        })
        .expect("draw");
    let _ = terminal.backend_mut().flush();
    t.elapsed().as_secs_f64() * 1000.0
}

fn draw_resize<B: Backend>(terminal: &mut Terminal<B>, run: &Run) -> f64 {
    let t = Instant::now();
    let status = run.status.clone();
    terminal
        .draw(|f| {
            let area = f.area();
            let body = ratatui::layout::Rect {
                height: area.height.saturating_sub(1),
                ..area
            };
            f.render_widget(Block::bordered().title(" resize-test "), body);
            // 填充内部，便于驱动侧检查 resize 后有没有残影。
            let inner = Block::bordered().inner(body);
            for y in inner.y..inner.bottom() {
                for x in inner.x..inner.right() {
                    f.buffer_mut()[(x, y)].set_char('X');
                }
            }
            let bar = Line::from(format!(" {status} "));
            f.render_widget(
                Paragraph::new(bar).style(Style::default().fg(Color::Black).bg(Color::Green)),
                ratatui::layout::Rect {
                    y: area.height.saturating_sub(1),
                    height: 1,
                    ..area
                },
            );
        })
        .expect("draw");
    let _ = terminal.backend_mut().flush();
    t.elapsed().as_secs_f64() * 1000.0
}

fn handle_event(run: &mut Run, ev: Event) {
    match ev {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) => match (code, modifiers.contains(KeyModifiers::CONTROL)) {
            (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => run.quit = true,
            (KeyCode::Char('p'), false) => run.panic = true,
            _ => {}
        },
        Event::Paste(s) => {
            run.paste_events += 1;
            run.paste_bytes += s.len();
            for b in s.bytes() {
                run.paste_sum = run.paste_sum.wrapping_add(b as u64);
            }
            run.status = format!(
                "paste:{}:{}:{}",
                run.paste_bytes, run.paste_sum, run.paste_events
            );
        }
        Event::Resize(_, _) => {
            run.status = format!("size:{}x:resized", run.paste_bytes);
        }
        _ => {}
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize - 1;
    sorted[idx.min(sorted.len() - 1)]
}

fn write_stats(args: &Args, run: &Run, size: Option<(u16, u16)>) {
    let mut sorted = run.frame_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let stats = json!({
        "mode": format!("{:?}", args.mode).to_lowercase(),
        "backend": match args.backend { BackendKind::Pty => "pty", BackendKind::Test => "test" },
        "duration_secs": args.duration.as_secs(),
        "tokens_per_sec": args.tokens_per_sec,
        "tokens": run.tokens,
        "tokens_sent": run.stats_extra_sent,
        "frames": run.frame_ms.len(),
        "frame_p50_ms": percentile(&sorted, 0.50),
        "frame_p95_ms": percentile(&sorted, 0.95),
        "frame_max_ms": sorted.last().copied().unwrap_or(0.0),
        "paste_events": run.paste_events,
        "paste_bytes": run.paste_bytes,
        "paste_sum": run.paste_sum,
        "terminal_size": size.map(|(c, r)| json!([c, r])),
        "frame_ms": run.frame_ms,
    });
    let tmp = args.stats.with_extension("tmp");
    std::fs::write(&tmp, stats.to_string()).expect("write stats tmp");
    std::fs::rename(&tmp, &args.stats).expect("rename stats");
}

fn main() {
    let args = parse_args();
    let started = Instant::now();

    // panic 路径也要恢复终端：hook 先还原，再交给默认 hook 打印。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut guard = TtyGuard { active: true };
        guard.restore();
        default_hook(info);
    }));

    match args.backend {
        BackendKind::Pty => run_pty(&args, started),
        BackendKind::Test => run_test(&args, started),
    }
}

fn run_pty(args: &Args, started: Instant) {
    let mut guard = TtyGuard::enter();
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout())).expect("terminal");
    let size = terminal.size().ok().map(|s| (s.width, s.height));
    let mut run = Run::new(match args.mode {
        Mode::Stream => format!("stream:{}tok/s", args.tokens_per_sec),
        Mode::Idle => "idle".into(),
        Mode::Paste => "paste-waiting".into(),
        Mode::Width | Mode::Resize => "starting".into(),
    });

    // token 生产者：绝对节拍定时（这是测量用的定时，不是同步 sleep）。
    let (tx, rx) = mpsc::channel::<String>();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sent_tokens = std::sync::Arc::clone(&sent);
    if args.mode == Mode::Stream {
        let period = Duration::from_nanos(1_000_000_000 / args.tokens_per_sec.max(1));
        let sent = std::sync::Arc::clone(&sent);
        thread::spawn(move || {
            let start = Instant::now();
            let mut i = 0u64;
            loop {
                if tx
                    .send(WORDS[(i as usize) % WORDS.len()].to_string())
                    .is_err()
                {
                    return;
                }
                sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                i += 1;
                let next = start + period * (i as u32);
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                }
            }
        });
    }

    if args.mode == Mode::Width {
        run.frame_ms.push(draw_width(&mut terminal));
        write_stats(args, &run, size);
        loop {
            if event::poll(Duration::from_millis(50)).expect("poll") {
                let ev = event::read().expect("read");
                let was_paste = matches!(ev, Event::Paste(_));
                handle_event(&mut run, ev);
                if run.quit || run.panic {
                    break;
                }
                if was_paste {
                    let _ = draw_width(&mut terminal);
                }
            }
            if started.elapsed() >= args.duration {
                break;
            }
        }
        guard.restore();
        write_stats(args, &run, size);
        if run.panic {
            panic!("spike panic (width)");
        }
        return;
    }

    let mut dirty = true;
    loop {
        // 输入：最多等一帧预算，避免空转也避免饿死绘制。
        let poll_budget = if args.mode == Mode::Idle {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(5)
        };
        if event::poll(poll_budget).expect("poll") {
            let ev = event::read().expect("read");
            handle_event(&mut run, ev);
            dirty = true;
        }
        // 流式：取走所有已到达的 token。
        if args.mode == Mode::Stream {
            let mut got = 0;
            while let Ok(word) = rx.try_recv() {
                append_token(&mut run, &word);
                run.tokens += 1;
                got += 1;
            }
            if got > 0 {
                dirty = true;
            }
        }
        if dirty {
            let took = match args.mode {
                Mode::Resize => {
                    let size = terminal.size().expect("size");
                    run.status = format!("size:{}x{}", size.width, size.height);
                    draw_resize(&mut terminal, &run)
                }
                _ => draw(&mut terminal, &run),
            };
            run.frame_ms.push(took);
            dirty = false;
        }
        if run.quit || run.panic || started.elapsed() >= args.duration {
            break;
        }
    }
    let exited_normally = run.quit || started.elapsed() >= args.duration;
    guard.restore();
    run.stats_extra_sent = sent_tokens.load(std::sync::atomic::Ordering::Relaxed);
    write_stats(args, &run, size);
    if run.panic {
        panic!("spike panic");
    }
    let _ = exited_normally;
}

/// TestBackend：不碰 tty，纯测布局 + 绘制的 CPU 成本。
fn run_test(args: &Args, started: Instant) {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    let mut run = Run::new(format!("stream:{}tok/s(test)", args.tokens_per_sec));
    let (tx, rx) = mpsc::channel::<String>();
    let period = Duration::from_nanos(1_000_000_000 / args.tokens_per_sec.max(1));
    thread::spawn(move || {
        // 与 pty 路径同款绝对节拍。
        let start = Instant::now();
        let mut i = 0u64;
        loop {
            if tx.send("tok".to_string()).is_err() {
                return;
            }
            i += 1;
            let next = start + period * (i as u32);
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            }
        }
    });
    loop {
        let mut got = 0;
        // 与 pty 路径同节奏：5ms 预算内合并到达的 token 后画一帧。
        let deadline = Instant::now() + Duration::from_millis(5);
        while Instant::now() < deadline {
            if let Ok(word) = rx.try_recv() {
                append_token(&mut run, &word);
                run.tokens += 1;
                got += 1;
            } else {
                thread::sleep(Duration::from_micros(200));
            }
        }
        if got > 0 {
            run.frame_ms.push(draw(&mut terminal, &run));
        }
        if started.elapsed() >= args.duration {
            break;
        }
    }
    write_stats(args, &run, Some((120, 40)));
}

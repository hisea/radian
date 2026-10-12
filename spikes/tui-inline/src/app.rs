//! Phase 0b spike：TUI inline 模式原型。**不要在自己的终端里直接运行**——虽然它
//! 不进备用屏，但会改终端状态、持续输出。全部经 pty 驱动（driver/lib.rs）。
//!
//! 关键实现约束（对任务单"前提"的落实）：
//! - 不使用 `ratatui::init_with_options`/`ratatui::restore`：后者**无条件**发
//!   `LeaveAlternateScreen`（`?1049l`），会让字节流出现 `?1049`。本原型自管初始化
//!   与恢复：raw mode + bracketed paste 开/关成对，**从不**发任何 `?1049`。
//! - 不开启任何鼠标上报（crossterm 的鼠标捕获一个都不用），终端原生选择不被劫持。
//! - viewport 用 `Viewport::Inline(height)`；插入 scrollback 用 `insert_before`；
//!   布局一律以 `Frame::area()` 为根（inline 原点可能非 (0,0)）。

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{TerminalOptions, Viewport};
use serde_json::json;

/// 恢复终端：只关自己开过的东西。绝无 `?1049`。
fn restore_tty() {
    use ratatui::crossterm::cursor::Show;
    use ratatui::crossterm::event::DisableBracketedPaste;
    let mut out = io::stdout();
    // ratatui 每帧 hide_cursor（draw 的固定行为），退出时必须补 Show。
    let _ = execute!(out, DisableBracketedPaste, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

fn enter_tty() -> io::Result<()> {
    use ratatui::crossterm::event::EnableBracketedPaste;
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnableBracketedPaste)?;
    Ok(())
}

struct Args {
    mode: String,
    height: u16,
    lines: usize,
    interval_ms: u64,
    duration_secs: u64,
    stats: PathBuf,
}

fn parse_args() -> Args {
    let mut a = Args {
        mode: "stream".into(),
        height: 5,
        lines: 30,
        interval_ms: 50,
        duration_secs: 60,
        stats: PathBuf::from("stats.json"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().expect("missing value");
        match k.as_str() {
            "--mode" => a.mode = v(),
            "--height" => a.height = v().parse().expect("height"),
            "--lines" => a.lines = v().parse().expect("lines"),
            "--interval-ms" => a.interval_ms = v().parse().expect("interval"),
            "--duration-secs" => a.duration_secs = v().parse().expect("duration"),
            "--stats" => a.stats = PathBuf::from(v()),
            other => panic!("unknown arg {other:?}"),
        }
    }
    a
}

fn render_status(f: &mut ratatui::Frame, pushed: usize, total: usize) {
    // inline viewport 原点可能非 (0,0)：一律以 f.area() 为根。
    let area = f.area();
    let (w, h) = (area.width, area.height);
    let bar = Line::from(vec![
        Span::styled(
            format!(" size:{w}x{h} "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(format!(" pushed:{pushed}/{total} ")),
        Span::styled(
            "q quit  p panic",
            Style::default().add_modifier(ratatui::style::Modifier::DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(bar), area);
}

fn main() {
    let args = parse_args();
    let started = Instant::now();

    // 自管 panic hook：先复原（不开的东西不开、关掉开过的），再交回默认 hook。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_tty();
        default_hook(info);
    }));

    assert!(enter_tty().is_ok(), "进入 raw mode 失败（应在 pty 里运行）");
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(args.height),
        },
    )
    .expect("terminal");

    let (tx, rx) = mpsc::channel::<usize>();
    {
        // 行推送节拍（测量用定时，不是同步 sleep）。
        let (lines, interval) = (args.lines, Duration::from_millis(args.interval_ms));
        thread::spawn(move || {
            let start = Instant::now();
            let mut i = 0usize;
            loop {
                if tx.send(i).is_err() {
                    return;
                }
                i += 1;
                if i >= lines {
                    return;
                }
                let next = start + interval * (i as u32);
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                }
            }
        });
    }

    let mut pushed = 0usize;
    let mut insert_ms: Vec<f64> = Vec::new();
    let mut frames = 0usize;
    let mut observed = (0u16, 0u16); // viewport 实际宽高（clamp 探测用）
    let mut quit = false;
    let mut panic_requested = false;
    let _ = &args.mode; // stream/resize 共用同一循环；resize 由驱动侧改 pty 尺寸触发
    loop {
        let mut resized = false;
        while event::poll(Duration::from_millis(5)).expect("poll") {
            match event::read().expect("read") {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) => match (code, modifiers.contains(KeyModifiers::CONTROL)) {
                    (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => quit = true,
                    (KeyCode::Char('p'), false) => panic_requested = true,
                    _ => {}
                },
                Event::Resize(..) => resized = true,
                _ => {}
            }
        }
        // 到点的行先插入 scrollback（viewport 上方），再重画 viewport。
        while pushed < args.lines {
            match rx.recv_timeout(Duration::from_millis(0)) {
                Ok(i) => {
                    let text = format!("LINE-{i:04} the quick brown fox jumps");
                    let t = Instant::now();
                    terminal
                        .insert_before(1, |buf| {
                            let line = Line::from(text.as_str());
                            let area = ratatui::layout::Rect {
                                width: buf.area.width,
                                height: 1,
                                ..buf.area
                            };
                            ratatui::widgets::Widget::render(line, area, buf);
                        })
                        .expect("insert_before");
                    insert_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                    pushed += 1;
                    let _ = i;
                }
                Err(_) => break,
            }
        }
        terminal
            .draw(|f| {
                observed = (f.area().width, f.area().height);
                render_status(f, pushed, args.lines);
            })
            .expect("draw");
        frames += 1;
        let _ = resized; // resize 后下一次 draw 自然按新宽度排版（ratatui autoresize）
        if quit || panic_requested || started.elapsed() >= Duration::from_secs(args.duration_secs) {
            break;
        }
    }

    let done_all = pushed >= args.lines;
    restore_tty();
    let mut sorted = insert_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| -> f64 {
        if sorted.is_empty() {
            0.0
        } else {
            let idx = ((sorted.len() as f64) * q).ceil() as usize - 1;
            sorted[idx.min(sorted.len() - 1)]
        }
    };
    let stats = json!({
        "mode": args.mode,
        "requested_height": args.height,
        "observed_viewport": [observed.0, observed.1],
        "lines_pushed": pushed,
        "all_lines_pushed": done_all,
        "frames": frames,
        "insert_count": insert_ms.len(),
        "insert_p50_ms": p(0.50),
        "insert_p95_ms": p(0.95),
        "insert_max_ms": sorted.last().copied().unwrap_or(0.0),
        "duration_secs": args.duration_secs,
    });
    let tmp = args.stats.with_extension("tmp");
    std::fs::write(&tmp, stats.to_string()).expect("write stats tmp");
    std::fs::rename(&tmp, &args.stats).expect("rename stats");

    if panic_requested {
        panic!("spike panic (inline)");
    }
    let _ = std::io::stdout().flush();
}

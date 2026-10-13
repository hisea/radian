//! Frame-time baseline for the transcript view (plan Phase 7).
//!
//! The TUI has **no virtualization**: `render::draw` re-wraps and re-styles every entry each
//! frame (see `crates/radian-tui/README.md`). This measures where that stops fitting in a
//! 60 fps budget (16.67 ms/frame) as the entry count grows.
//!
//! Measurement, not an assertion: `#[ignore]`d, so `cargo test` is unchanged. Run:
//!
//! ```sh
//! cargo test --release -p radian-tui --test perf_render -- --ignored --nocapture
//! ```
//!
//! Inputs are generated deterministically by [`entries`]: a coding-session mix of user and
//! assistant turns, reasoning, short tool calls, occasional long (2 KiB) tool results, and
//! Chinese text.

use std::time::{Duration, Instant};

use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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

fn measure(n: usize, samples: usize) -> (Duration, Duration) {
    let mut app = App::new("mock", "perf");
    app.entries = entries(n);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    // Warm up (layout + wrapping caches).
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        times.push(start.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    (median, p95)
}

#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn perf_render_frame_time() {
    let budget = Duration::from_millis(16);
    let mut first_over: Option<usize> = None;
    for n in [100usize, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000] {
        let samples = if n >= 10_000 { 5 } else { 20 };
        let (median, p95) = measure(n, samples);
        let over = median > budget || p95 > budget;
        println!(
            "entries {n:>6} @120x40: median {:.2} ms  p95 {:.2} ms  {}",
            median.as_secs_f64() * 1000.0,
            p95.as_secs_f64() * 1000.0,
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

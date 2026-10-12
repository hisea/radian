//! driver 入口：跑单个用例并往 `data/` 写证据文件。测试走同一套 lib 逻辑。

use std::path::PathBuf;

use tui_fullscreen_spike as lib;

fn data_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
    std::fs::create_dir_all(&d).expect("data dir");
    d
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("stream-pty") => {
            let d = data_dir();
            let stats = lib::case_stream("pty", 10, &d);
            println!(
                "pty 流式 10s: frames={} p50={:.2}ms p95={:.2}ms max={:.2}ms tokens={}",
                stats["frames"],
                stats["frame_p50_ms"],
                stats["frame_p95_ms"],
                stats["frame_max_ms"],
                stats["tokens"],
            );
        }
        Some("stream-test") => {
            let d = data_dir();
            let stats = lib::case_stream("test", 10, &d);
            println!(
                "TestBackend 流式 10s: frames={} p50={:.2}ms p95={:.2}ms max={:.2}ms tokens={}",
                stats["frames"],
                stats["frame_p50_ms"],
                stats["frame_p95_ms"],
                stats["frame_max_ms"],
                stats["tokens"],
            );
        }
        Some("paste") => {
            let d = data_dir();
            let (stats, latency_ms) = lib::case_paste(&d);
            println!(
                "paste: bytes={} sum={} events={} latency={:.0}ms max_frame={:.2}ms",
                stats["paste_bytes"],
                stats["paste_sum"],
                stats["paste_events"],
                latency_ms,
                stats["frame_max_ms"],
            );
            std::fs::write(d.join("paste-summary.txt"), format!(
                "5000 字节多行 bracketed paste；app 收到 bytes={} sum={}；发送→状态行显示 {:.0}ms；paste 期间最大帧 {:.2}ms\n",
                stats["paste_bytes"], stats["paste_sum"], latency_ms, stats["frame_max_ms"],
            )).unwrap();
        }
        Some("width") => {
            let d = data_dir();
            let report = lib::case_width(&d);
            print!("{report}");
            std::fs::write(d.join("width-samples.txt"), &report).unwrap();
        }
        Some("resize") => {
            let ok = lib::case_resize();
            println!("resize 无残影: {}", if ok { "通过" } else { "不通过" });
        }
        Some("restore-normal") => {
            lib::case_restore("normal");
            println!("restore(normal): 通过");
        }
        Some("restore-ctrlc") => {
            lib::case_restore("ctrlc");
            println!("restore(ctrl-c): 通过");
        }
        Some("restore-panic") => {
            lib::case_restore("panic");
            println!("restore(panic): 通过");
        }
        Some("idle-cpu") => {
            let d = data_dir();
            let samples = lib::case_idle_cpu(30, &d);
            let avg: f64 = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
            let max = samples.iter().cloned().fold(0.0, f64::max);
            println!(
                "idle CPU 30s: avg={avg:.2}% max={max:.2}% samples={}",
                samples.len()
            );
            let mut csv = String::from("sample,cpu_percent\n");
            for (i, v) in samples.iter().enumerate() {
                csv.push_str(&format!("{},{}\n", i + 1, v));
            }
            std::fs::write(d.join("idle-cpu.csv"), csv).unwrap();
        }
        other => {
            eprintln!(
                "usage: tui-driver <stream-pty|stream-test|paste|width|resize|restore-normal|restore-ctrlc|restore-panic|idle-cpu>（得到 {other:?}）"
            );
            std::process::exit(2);
        }
    }
}

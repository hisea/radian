//! driver：跑单个用例并往 data/ 写证据。

use std::path::PathBuf;

use tui_inline_spike as lib;

fn data_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
    std::fs::create_dir_all(&d).expect("data dir");
    d
}

fn dump(tag: &str, verdicts: &[lib::Verdict]) {
    print!("{tag}:\n{}", lib::render(verdicts));
    std::fs::write(
        data_dir().join(format!("{tag}.txt")),
        format!("{tag}:\n{}", lib::render(verdicts)),
    )
    .unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scrollback") => dump("scrollback", &lib::case_scrollback(&data_dir())),
        Some("resize") => dump("resize", &lib::case_resize(&data_dir())),
        Some("selection") => dump("selection", &lib::case_selection(&data_dir())),
        Some("prompt-normal") => dump("prompt-normal", &lib::case_prompt("normal")),
        Some("prompt-ctrlc") => dump("prompt-ctrlc", &lib::case_prompt("ctrlc")),
        Some("prompt-panic") => dump("prompt-panic", &lib::case_prompt("panic")),
        Some("clamp") => {
            let d = data_dir();
            // 高度钳制：请求 50 行 viewport（终端 40 行）与请求 10 行（终端 6 行）。
            let a = lib::case_clamp(&d, 50, 40, 120, "h50-term40");
            let b = lib::case_clamp(&d, 10, 6, 120, "h10-term6");
            let c = lib::case_clamp(&d, 3, 40, 120, "h3-term40");
            for (tag, v) in [("h50-term40", a), ("h10-term6", b), ("h3-term40", c)] {
                println!(
                    "{tag}: requested={} observed={}x{} insert p50={:.2}ms p95={:.2}ms max={:.2}ms ({} 次)",
                    v["requested_height"],
                    v["observed_viewport"][0],
                    v["observed_viewport"][1],
                    v["insert_p50_ms"],
                    v["insert_p95_ms"],
                    v["insert_max_ms"],
                    v["insert_count"],
                );
            }
        }
        other => {
            eprintln!(
                "usage: tui-inline-driver <scrollback|resize|selection|prompt-normal|prompt-ctrlc|prompt-panic|clamp>（得到 {other:?}）"
            );
            std::process::exit(2);
        }
    }
}

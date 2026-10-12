//! CLI 入口：`run`（全部用例 + 放行矩阵 + 写证据文件）、`bench`（启动开销）、
//! `probe ...`（沙箱内探针，见 lib::probe）。

use std::path::PathBuf;

use macos_sandbox_spike::{Rig, median, p95, probe, render_reports, with_timeout};

fn fixture_bin() -> PathBuf {
    // 根 workspace 构建产物里的 fixture；只读使用。
    if let Ok(p) = std::env::var("MCP_FIXTURE") {
        let p = PathBuf::from(p);
        assert!(p.exists(), "MCP_FIXTURE={} 不存在", p.display());
        return p;
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mcp-fixture");
    assert!(
        p.exists(),
        "找不到 mcp-fixture（{}）；先在仓库根 cargo build -p agent-extensions --bin mcp-fixture，\
         或用 MCP_FIXTURE 指定路径",
        p.display()
    );
    p
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => std::process::exit(probe(&args[1..])),
        Some("run") => {
            let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
            let probe = std::env::current_exe().expect("current exe");
            let mut rig = Rig::new(&data, fixture_bin(), probe);
            let cases = with_timeout(|| rig.run_all(), 120);
            print!("{}", render_reports(&cases));

            let matrix = rig.allow_matrix();
            let matrix_path = data.join("allow-matrix.json");
            std::fs::write(&matrix_path, serde_json::to_string_pretty(&matrix).unwrap()).unwrap();
            println!("放行矩阵已写 {}", matrix_path.display());

            let results = serde_json::to_string_pretty(&serde_json::json!({
                "cases": cases
                    .iter()
                    .map(|c| serde_json::json!({
                        "group": c.group, "name": c.name,
                        "expected": match c.expected {
                            macos_sandbox_spike::Expect::MustSucceed => "succeed",
                            macos_sandbox_spike::Expect::MustBeDenied => "denied",
                        },
                        "passed": c.passed, "detail": c.detail,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .unwrap();
            std::fs::write(data.join("results.json"), results).unwrap();
            std::fs::write(data.join("results.txt"), render_reports(&cases)).unwrap();
        }
        Some("bench") => {
            let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
            let probe = std::env::current_exe().expect("current exe");
            let mut rig = Rig::new(&data, fixture_bin(), probe);
            let (bare, sandboxed) = with_timeout(|| rig.bench_startup(20), 120);
            println!(
                "无沙箱   /usr/bin/true: median={:.2}ms p95={:.2}ms",
                median(&bare),
                p95(&bare)
            );
            println!(
                "sandbox-exec /usr/bin/true: median={:.2}ms p95={:.2}ms",
                median(&sandboxed),
                p95(&sandboxed)
            );
            println!(
                "开销（中位数差）: {:.2}ms",
                median(&sandboxed) - median(&bare)
            );
            let mut csv = String::from("run,bare_ms,sandbox_ms\n");
            for (i, (b, s)) in bare.iter().zip(&sandboxed).enumerate() {
                csv.push_str(&format!("{},{:.3},{:.3}\n", i + 1, b, s));
            }
            std::fs::write(data.join("startup-overhead.csv"), csv).unwrap();
            println!("数据已写 {}", data.join("startup-overhead.csv").display());
        }
        _ => {
            eprintln!("usage: macos-sandbox-spike <run|bench|probe ...>");
            std::process::exit(2);
        }
    }
}

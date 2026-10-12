//! Runs the escape matrix and writes evidence. Cross-compiled for the Linux VM:
//!
//! ```sh
//! RUSTFLAGS="-C link-self-contained=yes" \
//!   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$SYSROOT/lib/rustlib/aarch64-apple-darwin/bin/rust-lld \
//!   cargo build -p radian-sandbox --bin sandbox-escape --features testing \
//!   --target aarch64-unknown-linux-musl
//! scp target/aarch64-unknown-linux-musl/debug/sandbox-escape <user>@<vm>:/tmp/
//! ssh <user>@<vm> '/tmp/sandbox-escape --out /tmp/radian-sandbox-evidence'
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut out: Option<String> = None;
    let mut bench = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bench" => bench = true,
            "--out" => out = args.next(),
            "--help" | "-h" => {
                println!("usage: sandbox-escape [--out <dir>] [--bench]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    if bench {
        return match radian_sandbox::harness::startup_overhead(20) {
            Ok(overhead) => {
                println!("{}", overhead.report());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("sandbox backend unavailable: {e}");
                ExitCode::from(2)
            }
        };
    }

    let matrix = match radian_sandbox::harness::run_matrix() {
        Ok(matrix) => matrix,
        Err(e) => {
            // A requested sandbox with no working backend is not a test failure to hide.
            eprintln!("sandbox backend unavailable: {e}");
            return ExitCode::from(2);
        }
    };

    print!("{}", matrix.render_text());
    if let Some(dir) = out {
        let _ = std::fs::create_dir_all(&dir);
        let os = std::env::consts::OS;
        let _ = std::fs::write(format!("{dir}/escape-{os}.txt"), matrix.render_text());
        if let Ok(json) = serde_json::to_string_pretty(&matrix.to_json()) {
            let _ = std::fs::write(format!("{dir}/escape-{os}.json"), json);
        }
    }
    if matrix.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

//! The `radian` binary: the full-screen TUI. The command itself lives in [`radian_tui::cli`];
//! this file only enters it, so the `rad` alias can share the code without a source file
//! belonging to two build targets.

fn main() -> std::process::ExitCode {
    radian_tui::cli::run_cli()
}

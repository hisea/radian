//! The `rad` alias for `radian` (ADR 0026 slice 0b, ADR 0032): the same entry point as the
//! `radian` binary, reached through [`radian_tui::cli::run_cli`].

fn main() -> std::process::ExitCode {
    radian_tui::cli::run_cli()
}

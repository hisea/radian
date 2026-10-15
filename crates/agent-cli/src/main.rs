//! The `agent` binary: a thin wrapper over the `agent-cli` library (ADR 0032 slice 1).
//!
//! Everything it does lives in the library, so `radian exec` runs the same parser and the
//! same dispatch. The binary exists only so the two entries cannot drift.

fn main() {
    let cli = <agent_cli::Cli as clap::Parser>::parse();
    std::process::exit(agent_cli::run_blocking(cli));
}

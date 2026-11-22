//! The management commands (`radian trust`, `radian config import-pi`, `radian login`): the
//! subcommand grammar and the dispatch that runs before any model, key, session file or
//! network exists (ADR 0008).
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::path::PathBuf;

use radian_host::resolve_workspace;

use super::{Cli, CommandError, OutputError, finish};

/// The `--format` value for `--export`. `Default` is the clap default (`markdown`), so the
/// library's `Cli::default()` matches what the parser produces when the flag is absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ExportFormat {
    #[default]
    Markdown,
    Json,
}

/// The top-level subcommands. The command also has flag-only modes (`--list`, `--export`,
/// `--search`); those stay flags for compatibility. The subcommand name comes from the
/// variant, not the field that stores it.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Show or change a directory's project trust decision. Needs no model, no key, no
    /// session file, no network.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// Configuration tools: `radian config --help`.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Store an API key for a provider in ~/.radian/credentials/auth.json.
    /// The key is not echoed; a verification request (`GET <base>/models`) runs first.
    #[command(
        after_help = "Without a terminal the command refuses; edit credentials/auth.json directly instead."
    )]
    Login {
        /// A built-in provider id (`opencode-go`, `anthropic`, `openai`,
        /// `openai-compatible`), or `custom` to name a new endpoint (name, protocol, base
        /// URL, key). Omitted: a numbered list is shown.
        provider: Option<String>,
    },
    /// Remove a stored credential (asks first; every other entry is untouched). With a key
    /// name, remove just that key without asking.
    Logout {
        /// The provider id the key is stored under (`radian login` printed it).
        provider: String,
        /// The key's name (`first`, a `keys[]` name, or its masked form): remove that one
        /// without asking.
        name: Option<String>,
    },
}

/// The `radian trust` grammar. `status` reads the current directory's decision and its
/// source; `on`/`off` save it; `forget` removes it; `list` prints every saved decision.
/// A write changes disk state only: loaded resources stay as they are until the next start.
#[derive(clap::Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustCommand {
    /// Show the current directory's trust decision and its source.
    Status,
    /// Trust the current directory (saved; takes effect at the next start).
    On,
    /// Distrust the current directory (saved; takes effect at the next start).
    Off,
    /// Remove the current directory's saved decision (an ancestor's decision then
    /// applies, or none at all; takes effect at the next start).
    Forget,
    /// Print every saved trust decision, one per line.
    List,
}

/// The `radian config` grammar. `import-pi` is the only entry point in this version, and it is
/// read-only unless `--write` is given.
#[derive(clap::Subcommand, Debug)]
pub enum ConfigCommand {
    /// Draft a Radian configuration from a Pi agent directory: the draft goes to stdout and
    /// the mapping report to stderr. Nothing is written without `--write`.
    ImportPi {
        /// The Pi agent directory to read (for example `~/.pi/agent`). There is no default:
        /// the command never reads a directory you did not name.
        #[arg(long, value_name = "DIR")]
        from: PathBuf,
        /// Write the draft to the Radian configuration path (the `--config`/`RADIAN_CONFIG`
        /// file, or `~/.radian/config/config.toml`) instead of printing it. Refuses to
        /// overwrite an existing file.
        #[arg(long)]
        write: bool,
    },
}

pub(crate) fn run_trust(cli: &Cli, command: TrustCommand) -> i32 {
    finish(trust_command(cli, command))
}

pub(crate) fn run_config(cli: &Cli, command: ConfigCommand) -> i32 {
    finish(config_command(cli, command))
}

/// Runs one `radian config` subcommand. Deliberately before `setup`: no model, no key, no
/// session file and no network. The Pi directory is only ever the one the user named.
fn config_command(cli: &Cli, command: ConfigCommand) -> Result<(), OutputError> {
    match command {
        ConfigCommand::ImportPi { from, write } => {
            let draft = radian_host::import_pi(&from)?;
            // The report goes to stderr, so `radian config import-pi --from d > draft.toml`
            // still shows what could not be mapped.
            for note in &draft.notes {
                eprintln!("[import-pi] {note}");
            }
            if !write {
                radian_host::write_stdout(&draft.toml)?;
                eprintln!(
                    "[import-pi] nothing was written: this is a draft (pass --write to save it)"
                );
                return Ok(());
            }
            let env = std::env::var(radian_host::RADIAN_CONFIG_ENV).ok();
            let home = radian_host::radian_home();
            let path = radian_host::resolve_config_path(
                cli.config.as_deref(),
                env.as_deref(),
                home.as_deref(),
            )
            .ok_or_else(|| OutputError::Failed {
                message: "cannot resolve the Radian configuration path: pass --config, set \
                          RADIAN_CONFIG, or set HOME"
                    .into(),
                code: 1,
            })?;
            radian_host::write_pi_draft(&path, &draft.toml)?;
            eprintln!("[import-pi] wrote {}", path.display());
            Ok(())
        }
    }
}

/// Runs one `radian trust` subcommand. Deliberately before `setup`: no model, no key, no
/// session file, no network. `status` reads the current directory's decision and its
/// source; `on`/`off`/`forget` change `trust.json` (one audit-shaped write, no journal:
/// a management command has no session); `list` prints every saved decision. Every write
/// goes through `radian_host::write_line`, so a closed pipe exits 0 with no panic.
fn trust_command(cli: &Cli, command: TrustCommand) -> Result<(), OutputError> {
    use radian_host::trust;
    // `list` is about every saved decision, not the current directory: it resolves
    // nothing.
    if command == TrustCommand::List {
        let store = trust::read().map_err(CommandError::failure)?;
        if store.is_empty() {
            radian_host::write_line("no saved trust decisions")?;
            return Ok(());
        }
        for (path, decision) in &store {
            radian_host::write_line(&format!("{path}: {}", if *decision { "on" } else { "off" }))?;
        }
        return Ok(());
    }
    let workspace = resolve_workspace(cli.workspace.as_deref().map(std::path::Path::new))
        .map_err(CommandError::usage)?;
    match command {
        TrustCommand::List => unreachable!("handled above"),
        TrustCommand::Status => {
            let line = trust::describe(&workspace, None).map_err(CommandError::failure)?;
            radian_host::write_line(&line)?;
            Ok(())
        }
        TrustCommand::On | TrustCommand::Off | TrustCommand::Forget => {
            let write = match command {
                TrustCommand::On => trust::TrustWrite::On,
                TrustCommand::Off => trust::TrustWrite::Off,
                TrustCommand::Forget => trust::TrustWrite::Forget,
                TrustCommand::Status | TrustCommand::List => unreachable!("handled above"),
            };
            let path = trust::trust_file().ok_or_else(|| {
                CommandError::usage("no RADIAN_HOME or HOME to place trust.json under")
            })?;
            // A management command has no session and no journal: the audit is a no-op.
            let audit = |_: &trust::ProjectTrustChanged| {};
            let (_, written) = trust::apply_write(&path, &workspace, write, &audit)
                .map_err(CommandError::failure)?;
            radian_host::write_line(&trust::write_notice(&workspace, write, written))?;
            Ok(())
        }
    }
}

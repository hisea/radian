//! The management commands (`radian memory`, `radian config import-pi`): the
//! subcommand grammar and the dispatch that runs before any model, key, session file or
//! network exists (ADR 0008, ADR 0021 D7).
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::io::Read;
use std::path::PathBuf;

use radian_host::memory::MemoryService;
use radian_host::memory::commands::{self, CommandError, MemoryCommand};
use radian_host::resolve_workspace;

use super::trust::confirm_or_refuse;
use super::{Cli, OutputError, finish};

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
    /// Manage durable memory (ADR 0021 D7).
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Configuration tools (ADR 0008): `radian config --help`.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Store an API key for a provider in ~/.radian/credentials/auth.json (ADR 0026 D3).
    /// The key is not echoed; a verification request (`GET <base>/models`) runs first.
    #[command(
        after_help = "Without a terminal the command refuses; edit                            credentials/auth.json directly instead."
    )]
    Login {
        /// A built-in provider id (`opencode-go`, `anthropic`, `openai`,
        /// `openai-compatible`), or `custom` to name a new endpoint (name, protocol, base
        /// URL, key). Omitted: a numbered list is shown.
        provider: Option<String>,
    },
    /// Remove a stored credential (asks first; every other entry is untouched).
    Logout {
        /// The provider id the key is stored under (`radian login` printed it).
        provider: String,
    },
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
    /// Draft a Radian configuration from Claude Code files: the draft goes to stdout and
    /// the mapping report to stderr. Nothing is written without `--write`.
    ImportClaude {
        /// The Claude home directory to read (for example `~/.claude`). There is no
        /// default: the command never reads a directory you did not name.
        #[arg(long, value_name = "DIR")]
        claude_home: Option<PathBuf>,
        /// A project directory whose `.claude/` tree is read alongside the home tree.
        /// There is no default. At least one of `--claude-home` and `--project` is
        /// required.
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
        /// Write the draft (configuration, skills, `AGENTS.md`) to the Radian paths instead
        /// of printing it. Nothing that exists is ever changed: the configuration is
        /// merged, and existing `AGENTS.md`/`SKILL.md` targets are skipped with one report
        /// line each.
        #[arg(long)]
        write: bool,
    },
}

pub(crate) fn run_memory(cli: &Cli, command: MemoryCommand) -> i32 {
    finish(memory_command(cli, command))
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
        ConfigCommand::ImportClaude {
            claude_home,
            project,
            write,
        } => {
            let draft = radian_host::import_claude(claude_home.as_deref(), project.as_deref())?;
            // The report goes to stderr, so a stdout redirect still shows what could not
            // be mapped.
            for note in &draft.notes {
                eprintln!("[import-claude] {note}");
            }
            if !write {
                radian_host::write_stdout(&render_claude_stdout(&draft))?;
                eprintln!(
                    "[import-claude] nothing was written: this is a draft (pass --write to save it)"
                );
                return Ok(());
            }
            let targets = resolve_claude_targets(cli, project.as_deref())?;
            if !draft.project_skills.is_empty() {
                eprintln!(
                    "[import-claude] project-level files are trust-gate resources (ADR 0031 T1): \
                     they will be asked about on the next start"
                );
            }
            let report = radian_host::write_claude_draft(&draft, &targets)?;
            for path in &report.written {
                eprintln!("[import-claude] wrote {}", path.display());
            }
            for line in &report.skipped {
                eprintln!("[import-claude] {line}");
            }
            Ok(())
        }
    }
}

/// The dry-run document: the config draft, then each skill file and `AGENTS.md` body under
/// a `# --- <target> ---` header, so a redirect captures every file that `--write` would
/// create.
fn render_claude_stdout(draft: &radian_host::ClaudeDraft) -> String {
    let mut out = draft.config_toml.clone();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for skill in &draft.user_skills {
        out.push_str(&format!(
            "\n# --- skill {name} (user: ~/.agents/skills/{name}/SKILL.md) ---\n{content}",
            name = skill.name,
            content = skill.content
        ));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    for skill in &draft.project_skills {
        out.push_str(&format!(
            "\n# --- skill {name} (project: .agents/skills/{name}/SKILL.md) ---\n{content}",
            name = skill.name,
            content = skill.content
        ));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(body) = &draft.user_agents_md {
        out.push_str("\n# --- user AGENTS.md (~/.radian/AGENTS.md) ---\n");
        out.push_str(body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Resolves the `--write` targets for `import-claude` from the environment. Pure except for
/// reading the environment, so the shape is testable by inspection.
fn resolve_claude_targets(
    cli: &Cli,
    project: Option<&std::path::Path>,
) -> Result<radian_host::ClaudeTargets, OutputError> {
    let env = std::env::var(radian_host::RADIAN_CONFIG_ENV).ok();
    let radian_home = radian_host::radian_home();
    let config_path = radian_host::resolve_config_path(
        cli.config.as_deref(),
        env.as_deref(),
        radian_home.as_deref(),
    )
    .ok_or_else(|| OutputError::Failed {
        message: "cannot resolve the Radian configuration path: pass --config, set \
                  RADIAN_CONFIG, or set HOME"
            .into(),
        code: 1,
    })?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let user_skills_dir =
        radian_host::agents_skills_dir_for(home.as_deref()).ok_or_else(|| OutputError::Failed {
            message: "cannot resolve ~/.agents/skills: set HOME".into(),
            code: 1,
        })?;
    let user_agents_path = radian_home
        .as_deref()
        .map(|root| root.join("AGENTS.md"))
        .ok_or_else(|| OutputError::Failed {
            message: "cannot resolve ~/.radian/AGENTS.md: set RADIAN_HOME or HOME".into(),
            code: 1,
        })?;
    Ok(radian_host::ClaudeTargets {
        config_path,
        user_skills_dir,
        user_agents_path,
        project_skills_dir: project.map(|dir| dir.join(".agents/skills")),
    })
}

/// Runs one `radian memory` subcommand. Deliberately before `setup`: no model, no key, no
/// session file, no network. The grammar and the execution live in
/// `radian_host::memory::commands`, shared byte-for-byte with the TUI's `/memory`.
fn memory_command(cli: &Cli, command: MemoryCommand) -> Result<(), OutputError> {
    let workspace = resolve_workspace(cli.workspace.as_deref().map(std::path::Path::new))
        .map_err(CommandError::usage)?;
    let root = radian_host::memory_dir()
        .ok_or_else(|| CommandError::usage("no RADIAN_HOME or HOME to place memory under"))?;
    let service = MemoryService::open(&workspace, root);

    let command = resolve_input(command)?;
    let json = command.json();
    let confirmed = command.yes();
    let first = commands::execute(&service, command.clone(), confirmed, json)?;
    let output = match (&first.needs_confirmation, confirmed) {
        (Some(prompt), false) => {
            confirm_or_refuse(prompt)?;
            commands::execute(&service, command, true, json)?
        }
        _ => first,
    };
    if let Some(note) = &output.note {
        eprintln!("{note}");
    }
    radian_host::write_stdout(&output.text)?;
    Ok(())
}

/// Resolves the arguments that name external input: `add`/`edit` read `-` from stdin, and
/// `import` reads `-` or a file. The shared executor only ever sees literal text.
fn resolve_input(command: MemoryCommand) -> Result<MemoryCommand, CommandError> {
    Ok(match command {
        MemoryCommand::Add {
            global,
            kind,
            key,
            content,
        } if content == "-" => MemoryCommand::Add {
            global,
            kind,
            key,
            content: read_stdin()?,
        },
        MemoryCommand::Edit {
            global,
            revision,
            id,
            content,
        } if content == "-" => MemoryCommand::Edit {
            global,
            revision,
            id,
            content: read_stdin()?,
        },
        MemoryCommand::Import {
            global,
            project,
            source,
        } => MemoryCommand::Import {
            global,
            project,
            source: read_import(&source)?,
        },
        other => other,
    })
}

fn read_stdin() -> Result<String, CommandError> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| CommandError::failure(format!("cannot read stdin: {e}")))?;
    Ok(text)
}

fn read_import(source: &str) -> Result<String, CommandError> {
    if source == "-" {
        read_stdin()
    } else {
        std::fs::read_to_string(source)
            .map_err(|e| CommandError::failure(format!("cannot read {source}: {e}")))
    }
}

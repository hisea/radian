//! The minimal slash-command framework (ADR 0021 D7; ADR 0009 keeps the TUI full-screen).
//!
//! A submitted line whose first non-space character is `/` is a command: it never reaches the
//! model, the transcript or the session journal. `//…` is the escape — one `/` is dropped and
//! the rest is sent as an ordinary message.
//!
//! [`parse`] only **parses**: running a command is an effect the event loop executes, so the
//! app stays pure (ADR 0023 U3). `/memory` runs the same `radian-host::memory` command set
//! `radian memory` uses (one clap grammar, one executor). Adding a command is one arm here.

use radian_host::memory::commands::{self, CommandOutput, MemoryCommand};

/// The `/help` text. Kept short; the full grammar is `radian_host::memory::commands`.
pub const HELP: &str = "\
/help                 list the commands
/queue <text>          queue a message for the next turn, even while one runs
/retry                resend the last message after a model error
/loop [--every <dur>] [--times N] [--for <dur>] [prompt]
                       repeat the prompt every turn until --times/--for (default 20 turns or
                       1h); a bare /loop toggles; --every waits between turns
/goal <objective>      set a persistent goal that repeats every turn until done or budgeted
                       (default 30 turns or 2h); /goal shows it, /goal pause|resume|drop
                       controls it, /goal budget <N turns|<dur>|<N> tokens> caps it
/copy [code]          copy the last assistant message (or its last code block)
/compact [note]       summarize the history now and keep the recent part (ADR 0018)
/sessions [query]     list this workspace's recent sessions, or search their text
/fork                 fork this session at its last turn boundary (ADR 0022)
/new                  start a new session here; the old session stays resumable
/theme <name>         switch the color theme and write it back to the config file
/model <provider>/<model>
                      switch the model at the next turn boundary (ADR 0026 D5)
/thinking <level>     set the thinking level (off…max); unsupported levels clamp (ADR 0026 D11)
/usage                show the current provider's usage, one line per window (ADR 0027)
/context              show the next request's estimated size by category (ADR 0015)
/auto on|off          auto-approve tool asks for this session (ADR 0026 D9)
/yolo                 alias of /auto
/access [workspace|full]
                      show or switch the permission tier (ADR 0038); the switch takes effect
                      at the next tool call, and `full` asks for confirmation
/export [md|json] [path] [--force]
                      export this session (default md, default <workspace>/radian-<id>.<ext>)
/memory <command>     durable memory: list, search, show, add, edit, archive, restore,
                      forget, purge, confirm, export, import, project, relink, split, merge
/skill:<name> [args]  run a skill: read its body into the message (ADR 0025)
/skills               list the discovered skills
/jobs                 open the inspector's jobs page (list) or stop one/all (ADR 0037)
/tools                open the inspector's tools page
/mcp                  open the inspector's mcp page
/hooks                open the inspector's hooks page
/panel                open the inspector on the last page
/login [provider]     store an API key in credentials/auth.json (ADR 0026 D3); custom
                      names a new endpoint (name, protocol, base URL, key)
/logout <provider>    remove a stored credential (asks first)
//…                   send a message that starts with `/`
!<command>            run a shell command in the sandbox (ADR 0033); `!!` keeps its output
                      out of the model's context";

/// A memory command waiting for the user's confirmation. Only the destructive commands
/// (`forget`, `purge`, `split`, `merge`) ask (ADR 0021 D7); the rest run immediately. `update`
/// builds one when the executor reports [`radian_host::memory::commands::CommandOutput::
/// needs_confirmation`].
#[derive(Debug)]
pub struct PendingCommand {
    pub command: MemoryCommand,
    pub prompt: String,
    /// A `/model` switch waiting behind this confirm (ADR 0026 D7): `y` compacts to this
    /// budget and then queues the route; `n` cancels. `None` for a `/memory` confirm.
    pub switch: Option<(String, String, u64)>,
}

/// What `/jobs` asked for (ADR 0037 decision 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobsRequest {
    /// `/jobs`: list the running background jobs.
    List,
    /// `/jobs kill <id>`: stop one job's process group.
    Kill(String),
    /// `/jobs kill all`: stop every running job.
    KillAll,
}

/// One page of the inspector overlay (`jobs | tools | mcp | hooks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorPage {
    Jobs,
    Tools,
    Mcp,
    Hooks,
}

impl InspectorPage {
    /// The four pages in tab order.
    pub const ALL: [InspectorPage; 4] = [
        InspectorPage::Jobs,
        InspectorPage::Tools,
        InspectorPage::Mcp,
        InspectorPage::Hooks,
    ];

    /// The tab label.
    pub fn label(self) -> &'static str {
        match self {
            InspectorPage::Jobs => "jobs",
            InspectorPage::Tools => "tools",
            InspectorPage::Mcp => "mcp",
            InspectorPage::Hooks => "hooks",
        }
    }
}

/// What one submitted `/…` line meant. Parsing only: running the command is the loop's job
/// (ADR 0023 U3), and whether a write needs confirmation is the executor's answer.
#[derive(Debug)]
pub enum Outcome {
    /// Show these lines as notices and keep the line out of the conversation.
    Notices(Vec<String>),
    /// `/queue <text>`: like a send, but always a followup — even while a turn runs
    /// (Enter steers then). The loop expands the `@` references first, like a send.
    Queue(String),
    /// `/retry`: resend the last user message after a model-error turn. The app decides
    /// (idle and the last turn failed); the text goes as-is, with no `@` expansion.
    Retry,
    /// `/loop` (continuation step 1-2): toggle the per-turn repeat, or arm/enable it with
    /// limits and an optional prompt. Flags come before the prompt; running it is the loop's
    /// job (an effect), like every other command.
    Loop(LoopCommand),
    /// `/goal` (continuation step 3, ADR 0047): the persistent goal. Parsing only; running
    /// it journals through the loop (an effect), like every other command.
    Goal(GoalCommand),
    /// `/copy [code]`: copy the last committed assistant message — or its last fenced code
    /// block — through the configured clipboard backend.
    Copy { code_only: bool },
    /// A `/memory` command to run through the shared executor.
    Memory(MemoryCommand),
    /// A manual `/compact` (ADR 0018): the host runs it on the runtime, between turns, and
    /// shows the outcome as a notice. `Some(instructions)` is the optional note.
    Compact(Option<String>),
    /// `/sessions [query]`: the host lists or searches this workspace's sessions. `Some` is a
    /// full-text query, `None` the recent list.
    Sessions(Option<String>),
    /// `/export …`: the host exports the current session and writes the file.
    Export(ExportRequest),
    /// `/fork`: the host forks the current (idle) session at its last turn boundary.
    Fork,
    /// `/new`: leave the loop; the host ends this session and opens a fresh one in the
    /// same workspace, then the loop runs again. No host work happens here (unlike
    /// `/fork`), so there is no `SessionOutcome`: setting the flag IS the execution.
    New,
    /// `/theme <name>` (ADR 0008 revision): switch the theme now and write `tui.theme` back
    /// to the config file. The loop loads the theme (I/O) and does the write-back.
    Theme(String),
    /// `/model <provider>/<model>` (ADR 0026 D5): the loop resolves the credential and the
    /// new budget, then the app decides direct queue or confirm-first (D7).
    Model(ModelSwitch),
    /// `/thinking <level>` (ADR 0026 D11): the loop clamps it against the current route's map.
    Thinking(String),
    /// `/usage` (ADR 0027): the current provider's usage snapshot, every
    /// window of it. The app reads the contribution store, so there is no effect.
    Usage,
    /// `/context` (backlog `tui-context-command`): the next request's estimated size by
    /// category, plus the reported line, the message count and the compactions. Read-only;
    /// the loop reads it from the facade, so it works mid-turn.
    Context,
    /// `/login [provider]`: open the credential wizard. `Some(id)` skips the provider
    /// choice (a built-in id or `custom`); `None` starts at the choice.
    Login(Option<String>),
    /// `/logout <provider>`: remove one stored credential, after a confirmation.
    Logout(String),
    /// `/auto on|off` (and `/yolo`, its alias): the in-session auto-approval switch
    /// (ADR 0026 D9). `true` = on.
    AutoToggle(bool),
    /// `/access [workspace|full]` (ADR 0038 slice 2): show the current tier and where it
    /// came from (`None`), or switch to a named tier (`Some`).
    Access(Option<String>),
    /// `/skill:<name> [args]` (ADR 0025 S3): the host reads the body and sends
    /// `<skill name=…>…</skill>` + `args` as one user message. `args` is the raw remainder,
    /// verbatim.
    Skill { name: String, args: String },
    /// `/skills`: list the discovered skills (name + description) as notices.
    Skills,
    /// `/jobs`: list the running background jobs, or stop one/all (ADR 0037 decision 4).
    /// A bare `/jobs` opens the inspector's jobs page; `kill` forms stay text notices.
    Jobs(JobsRequest),
    /// Open the inspector overlay on one page, or on the last page (`None` = `/panel`).
    Inspector(Option<InspectorPage>),
    /// A user's `!`/`!!` shell command (ADR 0033). The command string runs in the sandbox
    /// (the tier's shell); `exclude_from_context` (`!!`) keeps its output out of the model's
    /// context. Execution is the loop's effect.
    Bash {
        command: String,
        exclude_from_context: bool,
    },
    /// Not a command after all (an empty token list); the caller sends the line.
    Message,
}

// ---------- the command tree and its per-level completion (ADR 0023 U4) ----------

/// One completion candidate: what to insert and the one-line description shown beside it. The
/// item in effect carries `current`, drawn as ` ✓` after its label (ASCII ` *`); the marker is
/// never part of `description`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub insert: String,
    pub description: String,
    pub current: bool,
}

/// What a level needs from the host when it cannot list its candidates by itself. `update` never
/// does I/O (ADR 0023 U3), so the loop resolves this and feeds the answer back as
/// [`crate::app::Input::CompletionsReady`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// `/memory <command> <partial>`: the record ids that command can act on.
    MemoryRecords { command: String, partial: String },
    /// `/theme <partial>`: the built-in names plus the theme files in `~/.radian/themes/`.
    Themes { partial: String },
    /// `/model <provider>/<model>`: the logged-in providers before the slash, the models of
    /// one provider after it (ADR 0026 D5).
    ModelRoute {
        provider: Option<String>,
        partial: String,
    },
    /// `/thinking <level>`: only the levels the **current** model supports (ADR 0026 D11),
    /// ranked against `partial`.
    ThinkingLevels { partial: String },
    /// `/logout <partial>`: the providers with a stored credential (the `auth.json` entries).
    LoggedInProviders { partial: String },
    /// `/skill:<partial>`: the discovered skill names (ADR 0025 S3, ruling 7). The host owns
    /// discovery, so the app cannot list them.
    SkillNames { partial: String },
}

impl Fetch {
    /// The partial word the fetched candidates should be ranked against.
    pub fn partial(&self) -> &str {
        match self {
            Fetch::MemoryRecords { partial, .. }
            | Fetch::Themes { partial }
            | Fetch::ModelRoute { partial, .. }
            | Fetch::ThinkingLevels { partial }
            | Fetch::LoggedInProviders { partial, .. }
            | Fetch::SkillNames { partial } => partial,
        }
    }
}

/// What one argument level of a command offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Level {
    /// Candidates known here, ranked by [`rank`] against the partial word.
    Ready(Vec<Candidate>),
    /// The candidates need the host.
    Needs(Fetch),
    /// Free text: nothing to complete at this level.
    Free,
}

/// A pure completion source: the argument words already typed before this level, and the partial
/// word being typed at it.
pub type LevelSource = fn(&[&str], &str) -> Level;

/// One slash command: its name (without the slash), its `/help` line, and one source per
/// argument level (`levels[0]` completes the first argument, and so on).
///
/// A command is **data**, so `/model` (provider → model, ADR 0026) and `/login` (provider) hang
/// on this same shape later: a new command is one entry, a new level is one function.
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
    pub levels: &'static [LevelSource],
}

/// The built-in tree.
pub fn tree() -> &'static [Command] {
    static TREE: &[Command] = &[
        Command {
            name: "help",
            description: "list the commands",
            levels: &[],
        },
        Command {
            name: "queue",
            description: "queue a message for the next turn, even while one runs",
            levels: &[free_level],
        },
        Command {
            name: "retry",
            description: "resend the last message after a model error",
            levels: &[],
        },
        Command {
            name: "loop",
            description: "repeat the prompt every turn until --times/--for (default 20 turns or \
             1h); a bare /loop toggles",
            levels: &[free_level],
        },
        Command {
            name: "goal",
            description: "set a persistent goal that repeats every turn until done or budgeted",
            levels: &[goal_level],
        },
        Command {
            name: "copy",
            description: "copy the last assistant message (or its last code block)",
            levels: &[copy_level],
        },
        Command {
            name: "compact",
            description: "summarize the history now and keep the recent part (ADR 0018)",
            levels: &[free_level],
        },
        Command {
            name: "sessions",
            description: "list this workspace's recent sessions, or search their text",
            levels: &[free_level],
        },
        Command {
            name: "fork",
            description: "fork this session at its last turn boundary (ADR 0022)",
            levels: &[],
        },
        Command {
            name: "new",
            description: "start a new session here; the old session stays resumable",
            levels: &[],
        },
        Command {
            name: "theme",
            description: "switch the color theme and write it back to the config file",
            levels: &[theme_level],
        },
        Command {
            name: "model",
            description: "switch the model at the next turn boundary (ADR 0026 D5)",
            levels: &[model_route_level],
        },
        Command {
            name: "thinking",
            description: "set the thinking level (off…max); unsupported levels clamp (ADR 0026 D11)",
            levels: &[thinking_level_level],
        },
        Command {
            name: "usage",
            description: "show the current provider's usage, one line per window (ADR 0027)",
            levels: &[],
        },
        Command {
            name: "context",
            description: "show the next request's estimated size by category (ADR 0015)",
            levels: &[],
        },
        Command {
            name: "auto",
            description: "auto-approve tool asks for this session (ADR 0026 D9)",
            levels: &[auto_level],
        },
        Command {
            name: "yolo",
            description: "alias of /auto",
            levels: &[auto_level],
        },
        Command {
            name: "access",
            description: "show or switch the permission tier (ADR 0038)",
            levels: &[access_level],
        },
        Command {
            name: "export",
            description: "export this session (default md, default <workspace>/radian-<id>.<ext>)",
            levels: &[export_level, export_level],
        },
        Command {
            name: "memory",
            description: MEMORY_DESCRIPTION,
            levels: &[memory_level, memory_level],
        },
        Command {
            name: "skills",
            description: "list the discovered skills",
            levels: &[],
        },
        Command {
            name: "skill:",
            description: "run a skill: read its body into the message",
            levels: &[],
        },
        Command {
            name: "login",
            description: "store an API key in credentials/auth.json (ADR 0026 D3); custom \
                          names a new endpoint",
            levels: &[login_level],
        },
        Command {
            name: "logout",
            description: "remove a stored credential (asks first)",
            levels: &[logout_level],
        },
        Command {
            name: "jobs",
            description: "open the inspector's jobs page (list) or stop one/all",
            levels: &[jobs_level],
        },
        Command {
            name: "tools",
            description: "open the inspector's tools page",
            levels: &[],
        },
        Command {
            name: "mcp",
            description: "open the inspector's mcp page",
            levels: &[],
        },
        Command {
            name: "hooks",
            description: "open the inspector's hooks page",
            levels: &[],
        },
        Command {
            name: "panel",
            description: "open the inspector on the last page",
            levels: &[],
        },
    ];
    TREE
}

/// `/login [provider]`: the built-in descriptors plus `custom` (ADR 0026 D3/D10).
fn login_level(typed: &[&str], _partial: &str) -> Level {
    match typed.first() {
        None => Level::Ready(
            crate::login::LoginWizard::choices()
                .into_iter()
                .map(|(id, description)| Candidate {
                    insert: id,
                    description,
                    current: false,
                })
                .collect(),
        ),
        Some(_) => Level::Free,
    }
}

/// `/logout <provider>`: the candidates are the stored credentials, which only the host
/// knows (it reads `auth.json`).
fn logout_level(typed: &[&str], partial: &str) -> Level {
    match typed.first() {
        None => Level::Needs(Fetch::LoggedInProviders {
            partial: partial.to_owned(),
        }),
        Some(_) => Level::Free,
    }
}

/// The `/help` line for `/memory` (kept next to the subcommand table so the two cannot drift).
const MEMORY_DESCRIPTION: &str = "durable memory: list, search, show, add, edit, archive, \
                                  restore, forget, purge, confirm, export, import, project, \
                                  relink, split, merge";

/// The `/memory` subcommands and a one-line description each. The list is the authority for
/// completion; `the_memory_subcommand_table_matches_the_grammar` fails if it drifts from the
/// clap grammar in `radian_host::memory::commands`.
const MEMORY_SUBCOMMANDS: &[(&str, &str)] = &[
    ("list", "list records (Project then Global)"),
    ("search", "search records by keyword"),
    ("show", "show one record by id"),
    ("add", "add a record"),
    ("edit", "change a record's content"),
    ("archive", "archive a record"),
    ("restore", "restore an archived record"),
    ("forget", "forget a record (needs confirmation)"),
    (
        "purge",
        "rewrite the log, removing forgotten content (needs confirmation)",
    ),
    ("confirm", "confirm a record"),
    ("export", "print one scope as editable Markdown"),
    ("import", "import an edited Markdown export"),
    ("project", "show the project identity, or `project new`"),
    ("relink", "attach this workspace to an existing project"),
    (
        "split",
        "move records between projects (needs confirmation)",
    ),
    (
        "merge",
        "merge one project into another (needs confirmation)",
    ),
];

/// The `/memory` subcommands whose first argument is a record id.
const MEMORY_ID_COMMANDS: &[&str] = &["show", "edit", "archive", "restore", "forget", "confirm"];

/// `/memory`: the subcommand, then the record id for the commands that take one.
fn memory_level(typed: &[&str], partial: &str) -> Level {
    match typed.first().copied() {
        None => Level::Ready(
            MEMORY_SUBCOMMANDS
                .iter()
                .map(|(name, description)| Candidate {
                    insert: (*name).to_owned(),
                    description: (*description).to_owned(),
                    current: false,
                })
                .collect(),
        ),
        Some(command) if MEMORY_ID_COMMANDS.contains(&command) => {
            Level::Needs(Fetch::MemoryRecords {
                command: command.to_owned(),
                partial: partial.to_owned(),
            })
        }
        Some(_) => Level::Free,
    }
}

/// `/theme`: the built-in names (`dark`/`light`/`mono`) and the theme files in
/// `~/.radian/themes/` (ADR 0029). Listing the directory is I/O, so the loop answers. The
/// command takes exactly one name: once it is typed the level offers nothing, so the list
/// closes and Enter submits.
fn theme_level(typed: &[&str], partial: &str) -> Level {
    if typed.is_empty() {
        Level::Needs(Fetch::Themes {
            partial: partial.to_owned(),
        })
    } else {
        Level::Free
    }
}

/// The thinking level the footer's `reasoning` item shows (ADR 0026 D11): the requested level
/// clamped to what this route supports, so a model that does not support `medium` shows the
/// level that actually takes effect (`high`) instead of the configured value.
pub fn effective_thinking_level(
    provider: &radian_host::ProviderRef,
    model: &str,
    requested: &str,
    max_tokens: u32,
) -> String {
    radian_host::clamp_thinking_with(
        radian_host::thinking_map_for_ref(provider, model),
        requested,
        Some(max_tokens),
    )
    .map(|(level, _)| level.to_owned())
    .unwrap_or_else(|| requested.to_owned())
}

/// `/thinking`'s completion rows (ADR 0026 D11): only the levels the route supports, each with
/// the wire value it sends (`enabled:4096` for Anthropic, `high` for openai-compatible); the
/// applied level carries `current` (drawn as ` ✓` after its name, ).
pub fn thinking_candidates(
    provider: &radian_host::ProviderRef,
    model: &str,
    max_tokens: u32,
    current: &str,
) -> Vec<Candidate> {
    let map = radian_host::thinking_map_for_ref(provider, model);
    radian_host::supported_thinking_levels_ref_with(provider, model, Some(max_tokens))
        .into_iter()
        .map(|level| {
            let wire = radian_host::clamp_thinking_with(map, level, Some(max_tokens))
                .map(|(_, wire)| wire)
                .unwrap_or("off");
            Candidate {
                insert: level.to_owned(),
                description: format!("sends {wire}"),
                current: level == current,
            }
        })
        .collect()
}

/// The `/theme` completion candidates: the built-ins plus the `*.json` files in `themes_dir`
/// (ADR 0029). Sorting makes the list stable; the app ranks it against the partial word.
pub fn theme_candidates(themes_dir: Option<&std::path::Path>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = ["dark", "light", "mono"]
        .into_iter()
        .map(|name| Candidate {
            insert: name.to_string(),
            description: "built-in theme".to_string(),
            current: false,
        })
        .collect();
    if let Some(dir) = themes_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let file = entry.file_name();
            let name = file.to_string_lossy();
            // ADR 0029: a Radian `.toml` theme, or the old pi `.json`
            // theme (still loadable this version); both are candidates.
            let stem = name
                .strip_suffix(".toml")
                .or_else(|| name.strip_suffix(".json"));
            if let Some(stem) = stem {
                out.push(Candidate {
                    insert: stem.to_string(),
                    description: "theme file".to_string(),
                    current: false,
                });
            }
        }
    }
    out.sort_by(|a, b| a.insert.cmp(&b.insert));
    out.dedup_by(|a, b| a.insert == b.insert);
    out
}

/// `/auto on|off` (and `/yolo`): the two words are the whole argument grammar (ADR 0026 D9).
fn auto_level(_typed: &[&str], _partial: &str) -> Level {
    Level::Ready(vec![
        Candidate {
            insert: "on".to_owned(),
            description: "approve tool asks without asking, this session".to_owned(),
            current: false,
        },
        Candidate {
            insert: "off".to_owned(),
            description: "ask again (back to on-ask)".to_owned(),
            current: false,
        },
    ])
}

/// `/access`: the two tier names (ADR 0038 slice 2).
fn access_level(_typed: &[&str], _partial: &str) -> Level {
    Level::Ready(vec![
        Candidate {
            insert: "workspace".to_owned(),
            description: "write the workspace; sandboxed shell".to_owned(),
            current: false,
        },
        Candidate {
            insert: "full".to_owned(),
            description: "no sandbox; asks for confirmation".to_owned(),
            current: false,
        },
    ])
}

/// `/jobs`: `kill` then the id (`all` or free text). A bare `/jobs` opens the panel.
fn jobs_level(typed: &[&str], _partial: &str) -> Level {
    match typed.first() {
        None => Level::Ready(vec![Candidate {
            insert: "kill".to_owned(),
            description: "stop one/all background jobs".to_owned(),
            current: false,
        }]),
        Some(_) => Level::Free,
    }
}

/// `/copy [code]`: the only argument is the `code` selector; past it, free text (nothing).
fn copy_level(typed: &[&str], _partial: &str) -> Level {
    if typed.is_empty() {
        Level::Ready(vec![Candidate {
            insert: "code".to_owned(),
            description: "only the last code block, without the fences".to_owned(),
            current: false,
        }])
    } else {
        Level::Free
    }
}

/// `/export`: the format, then the path (with `--force` offered as a candidate).
fn export_level(typed: &[&str], _partial: &str) -> Level {
    match typed.first() {
        None => Level::Ready(vec![
            Candidate {
                insert: "md".to_owned(),
                description: "markdown: the readable transcript".to_owned(),
                current: false,
            },
            Candidate {
                insert: "json".to_owned(),
                description: "json: the event document".to_owned(),
                current: false,
            },
        ]),
        Some(_) => Level::Ready(vec![Candidate {
            insert: "--force".to_owned(),
            description: "overwrite an existing file".to_owned(),
            current: false,
        }]),
    }
}

/// A level that takes free text: nothing to complete.
fn free_level(_typed: &[&str], _partial: &str) -> Level {
    Level::Free
}

/// The completion for `line` at `cursor`: the char range a candidate replaces, and what that
/// level offers. Pure. `None` when the line is not a slash command, the command or the level is
/// unknown, the level is free text, or the typed word is already exactly a candidate (see
/// [`is_exact_candidate`]).
///
/// `level` is 0 for the command name itself, 1 for the first argument, and so on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCompletion {
    pub level: usize,
    pub range: std::ops::Range<usize>,
    pub level_result: Level,
}

/// See [`SlashCompletion`]. `tree` is a parameter so a test (or a later `/model`) can supply its
/// own tree.
pub fn complete_in(tree: &[Command], line: &str, cursor: usize) -> Option<SlashCompletion> {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    if !before.starts_with('/') {
        return None;
    }
    // The words before the cursor with their char ranges; the last one is the partial word
    // (empty when the line ends in whitespace, and then the cursor is the range).
    let mut words: Vec<(String, std::ops::Range<usize>)> = Vec::new();
    let mut start = None;
    for (index, c) in before.chars().enumerate() {
        if c.is_whitespace() {
            if let Some(from) = start.take() {
                words.push((chars[from..index].iter().collect(), from..index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    let trailing = start.is_none() && !words.is_empty();
    if let Some(from) = start {
        words.push((chars[from..cursor].iter().collect(), from..cursor));
    }
    let (first, rest) = words.split_first()?;

    // A bare `/`: every command.
    if first.0 == "/" && rest.is_empty() {
        return Some(SlashCompletion {
            level: 0,
            range: 0..cursor,
            level_result: Level::Ready(command_candidates(tree)),
        });
    }

    // `/skill:<partial>` (ADR 0025 S3, ruling 7): the candidates are the discovered skill
    // names, which only the host knows. A dedicated branch, because the name is glued to the
    // command with `:` rather than separated by a space, so the argument-level machinery cannot
    // see it.
    let name = first.0.trim_start_matches('/');
    if let Some(partial) = name.strip_prefix("skill:") {
        return non_empty(SlashCompletion {
            level: 1,
            range: first.1.start + "/skill:".len()..first.1.end,
            level_result: Level::Needs(Fetch::SkillNames {
                partial: partial.to_owned(),
            }),
        });
    }

    // An exact command name: complete that command's levels one at a time.
    if let Some(command) = tree.iter().find(|command| command.name == name) {
        if trailing {
            // `/memory `: the *next* level's candidates, with an empty partial.
            let typed: Vec<&str> = rest.iter().map(|(word, _)| word.as_str()).collect();
            return argument_level(command, typed.len(), &typed, "", cursor, cursor);
        }
        let Some((partial, range)) = rest.last() else {
            return None; // only the name is typed: nothing more to complete
        };
        let typed_args: Vec<&str> = rest[..rest.len() - 1]
            .iter()
            .map(|(word, _)| word.as_str())
            .collect();
        return argument_level(
            command,
            typed_args.len(),
            &typed_args,
            partial,
            range.start,
            range.end,
        );
    }

    // A partial command name: chain the levels that each have exactly one fuzzy match, so
    // `/mem fo` completes to `/memory forget` in one step. A level that needs the host stops the
    // chain (its candidates are not known here).
    let matches = rank(command_candidates(tree), name);
    if matches.len() != 1 {
        return None;
    }
    let matched = matches[0].insert.trim_start_matches('/').to_owned();
    let command = tree.iter().find(|command| command.name == matched)?;
    let mut insert = format!("/{matched}");
    let mut end = first.1.end;
    let mut level = 0;
    for (index, (word, range)) in rest.iter().enumerate() {
        let Some(source) = command.levels.get(index) else {
            break;
        };
        let typed: Vec<&str> = rest[..index]
            .iter()
            .map(|(word, _)| word.as_str())
            .collect();
        let Level::Ready(candidates) = source(&typed, word) else {
            break;
        };
        let ranked = rank(candidates, word);
        if ranked.len() != 1 {
            break;
        }
        insert.push(' ');
        insert.push_str(&ranked[0].insert);
        level = index + 1;
        end = range.end;
    }
    if level == 0 && !trailing {
        // Only the name was completed; the range covers the first word alone, so the arguments
        // that follow are left for the next completion.
        end = first.1.end;
    }
    if insert.trim_end() == before.trim_end() {
        return None; // already exactly this: nothing to do
    }
    Some(SlashCompletion {
        level,
        range: 0..end,
        level_result: Level::Ready(vec![Candidate {
            insert,
            description: matches[0].description.clone(),
            current: matches[0].current,
        }]),
    })
}

/// The candidates of one argument level of `command`, ranked against `partial`. `None` when the
/// level is free text, unknown, or already exactly what was typed.
fn argument_level(
    command: &Command,
    level: usize,
    typed: &[&str],
    partial: &str,
    from: usize,
    to: usize,
) -> Option<SlashCompletion> {
    let source = command.levels.get(level)?;
    let level_result = match source(typed, partial) {
        Level::Ready(candidates) => {
            let candidates = rank(candidates, partial);
            if is_exact_candidate(&candidates, partial) {
                return None;
            }
            Level::Ready(candidates)
        }
        other => other,
    };
    non_empty(SlashCompletion {
        level: level + 1,
        range: from..to,
        level_result,
    })
}

/// The command-name candidates, from the tree's own `/help` lines.
fn command_candidates(tree: &[Command]) -> Vec<Candidate> {
    tree.iter()
        .map(|command| Candidate {
            insert: format!("/{}", command.name),
            description: command.description.to_owned(),
            current: false,
        })
        .collect()
}

/// `None` when there is nothing to show: a `Ready` level with no candidates, or a level that
/// takes free text.
fn non_empty(completion: SlashCompletion) -> Option<SlashCompletion> {
    match &completion.level_result {
        Level::Ready(candidates) if candidates.is_empty() => None,
        Level::Free => None,
        _ => Some(completion),
    }
}

/// Whether one of `candidates` is **already exactly** the typed `partial`. `rank` puts such a
/// candidate first, and it is not a completion to accept: arming the menu on it would make Enter
/// accept the row (inserting the very same word, or — when a longer id shares the prefix, as
/// `gpt-5.4` does with `gpt-5.4-mini` — a *different* model) instead of submitting the finished
/// line. Both completion paths ask this one question — the pure `Ready` levels and the app's
/// answer for a `Needs` level — so the rule cannot drift between them.
pub fn is_exact_candidate(candidates: &[Candidate], partial: &str) -> bool {
    candidates
        .iter()
        .any(|candidate| candidate.insert == partial)
}

/// Ranks `candidates` against `partial` with [`fuzzy_score`]: the best score first, then by name,
/// so the order is deterministic. An empty partial keeps the source's own order.
pub fn rank(mut candidates: Vec<Candidate>, partial: &str) -> Vec<Candidate> {
    if partial.is_empty() {
        return candidates;
    }
    let mut scored: Vec<(i32, Candidate)> = candidates
        .drain(..)
        .filter_map(|candidate| {
            fuzzy_score(&candidate.insert, partial).map(|score| (score, candidate))
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.insert.cmp(&right.1.insert))
    });
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

/// A deterministic fuzzy score: every character of `query` must appear in `candidate` in order
/// (case-insensitively; a leading `/` is ignored, so `/co` prefix-matches `/compact`).
///
/// `1000` for a prefix match, `+10` for each adjacent pair, `+1` per matched character, and `-1`
/// for the index of the first match: a prefix beats a scattered match, and an early, dense match
/// beats a late, sparse one. `None` when `query` is not a subsequence.
pub fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
    let candidate = candidate.trim_start_matches('/').to_lowercase();
    let query = query.trim_start_matches('/').to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let mut score = if candidate.starts_with(&query) {
        1000
    } else {
        0
    };
    let mut chars = candidate.char_indices();
    let mut first: Option<usize> = None;
    let mut previous: Option<usize> = None;
    let mut matched = 0;
    for wanted in query.chars() {
        loop {
            let (index, found) = chars.next()?;
            if found == wanted {
                if first.is_none() {
                    first = Some(index);
                }
                if previous == Some(index.saturating_sub(1)) {
                    score += 10;
                }
                previous = Some(index);
                matched += 1;
                break;
            }
        }
    }
    Some(score + matched - first.unwrap_or(0) as i32)
}

/// A parsed `/export` request. The host resolves the path, checks overwrite and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRequest {
    pub format: radian_host::Format,
    pub path: Option<String>,
    pub force: bool,
}

/// The host-level work a slash command asks for: neither touches the transcript, so the app
/// records the request and the event loop runs it with the session directory and secrets.
/// `/sessions` is no longer here: it opens the picker overlay ([`crate::app::Effect::
/// OpenPicker`]) from the loop, because listing is I/O the loop owns (ADR 0023 U3/U4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    Export(ExportRequest),
    Fork,
}

/// One `/model` target: the provider id (a descriptor, or `auth.json`'s named provider) and
/// the model id. Nothing else travels with it (ADR 0026 D5: no key, no header values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSwitch {
    pub provider: String,
    pub model: String,
}

/// Splits `/model`'s argument: `provider/model` in one word, or `provider model` in two.
/// Returns `None` (with a usage notice) when either half is missing.
pub fn model_switch(args: &[String]) -> Result<ModelSwitch, String> {
    let joined = args.join(" ");
    let joined = joined.trim();
    if let Some((provider, model)) = joined.split_once('/') {
        return Ok(ModelSwitch {
            provider: provider.trim().to_owned(),
            model: model.trim().to_owned(),
        });
    }
    match (args.first(), args.get(1)) {
        (Some(provider), Some(model)) => Ok(ModelSwitch {
            provider: provider.clone(),
            model: model.clone(),
        }),
        _ => Err("usage: /model <provider>/<model>".to_owned()),
    }
}

/// `/model`'s one argument level: the flat list of every logged-in provider's models
/// (ADR 0035). The candidates need the host's credential store and the model
/// lists, so the level is `Needs` and the loop resolves it. The direct `/model
/// <provider>/<model>` form still works: the typed word is passed as the partial, so a full
/// `provider/model` is an exact candidate and is not armed — never, not even when a longer id
/// shares its prefix.
fn model_route_level(_typed: &[&str], partial: &str) -> Level {
    Level::Needs(Fetch::ModelRoute {
        provider: None,
        partial: partial.to_owned(),
    })
}

/// `/thinking`'s one level: only the levels the current model supports are offered
/// (ADR 0026 D11), which the loop resolves from the route's map.
fn thinking_level_level(_typed: &[&str], partial: &str) -> Level {
    Level::Needs(Fetch::ThinkingLevels {
        partial: partial.to_owned(),
    })
}

/// `/skill:<name> [args]` (ADR 0025 S3). The name is the rest of the first word after the
/// colon; the arguments are the raw remainder of the line, verbatim (so quotes survive). An
/// empty name is a usage notice.
fn skill_command(first: &str, line: &str) -> Outcome {
    let name = first.trim_start_matches("/skill:").to_owned();
    if name.is_empty() {
        return Outcome::Notices(vec!["usage: /skill:<name> [args]".to_owned()]);
    }
    let args = line
        .trim()
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim().to_owned())
        .unwrap_or_default();
    Outcome::Skill { name, args }
}

/// Interprets a trimmed line that starts with `/`. `memory_enabled` says whether this process
/// has a memory root; without one `/memory` is refused with a notice. Pure: it never runs a
/// command (ADR 0023 U3).
pub fn parse(line: &str, memory_enabled: bool) -> Outcome {
    // ADR 0033: `!`/`!!` comes **before** the `/…` commands, so `!/bin/sh` is a command, not
    // the unknown `/bin/sh`. The rest of the line is the shell command verbatim (never
    // shlexed here: the shell its own quoting); only surrounding whitespace is trimmed.
    if line.starts_with('!') {
        let (exclude_from_context, rest) = match line.strip_prefix("!!") {
            Some(rest) => (true, rest),
            None => (false, line.strip_prefix('!').unwrap_or(line)),
        };
        let command = rest.trim();
        if command.is_empty() {
            return Outcome::Notices(vec![
                "usage: !<command> runs it in the sandbox (!!<command> keeps its output out of \
                 the model's context)"
                    .to_owned(),
            ]);
        }
        return Outcome::Bash {
            command: command.to_owned(),
            exclude_from_context,
        };
    }
    let tokens = match shlex::split(line) {
        Some(tokens) => tokens,
        None => return Outcome::Notices(vec![format!("unbalanced quotes in {line:?}")]),
    };
    match tokens.first().map(String::as_str) {
        Some("/help") => Outcome::Notices(vec![HELP.to_owned()]),
        Some("/queue") => queue_text(&tokens[1..]),
        Some("/retry") if tokens.len() == 1 => Outcome::Retry,
        Some("/retry") => Outcome::Notices(vec!["usage: /retry (no arguments)".to_owned()]),
        Some("/loop") => loop_request(&tokens[1..]),
        Some("/goal") => goal_request(&tokens[1..]),
        Some("/copy") => copy_request(&tokens[1..]),
        Some("/memory") => memory_command(&tokens[1..], memory_enabled),
        Some("/compact") => Outcome::Compact(compact_instructions(&tokens[1..])),
        Some("/sessions") => Outcome::Sessions(sessions_query(&tokens[1..])),
        Some("/export") => export_request(&tokens[1..]),
        Some("/fork") => Outcome::Fork,
        Some("/new") if tokens.len() == 1 => Outcome::New,
        Some("/new") => Outcome::Notices(vec!["usage: /new (no arguments)".to_owned()]),
        Some("/theme") => theme_name(&tokens[1..]),
        Some("/thinking") => match tokens[1..].first() {
            Some(level) => Outcome::Thinking(level.clone()),
            None => Outcome::Notices(vec!["usage: /thinking <level>".to_owned()]),
        },
        Some("/usage") if tokens.len() == 1 => Outcome::Usage,
        Some("/usage") => Outcome::Notices(vec!["usage: /usage (no arguments)".to_owned()]),
        Some("/context") if tokens.len() == 1 => Outcome::Context,
        Some("/context") => Outcome::Notices(vec!["usage: /context (no arguments)".to_owned()]),
        Some("/model") => match model_switch(&tokens[1..]) {
            Ok(switch) => Outcome::Model(switch),
            Err(usage) => Outcome::Notices(vec![usage]),
        },
        Some("/login") => Outcome::Login(login_provider(&tokens[1..])),
        Some("/logout") => Outcome::Logout(logout_provider(&tokens[1..])),
        Some("/auto") | Some("/yolo") => auto_toggle(&tokens[1..]),
        Some("/access") => access_request(&tokens[1..]),
        Some("/skills") if tokens.len() == 1 => Outcome::Skills,
        Some("/skills") => Outcome::Notices(vec!["usage: /skills (no arguments)".to_owned()]),
        Some("/jobs") => jobs_request(&tokens[1..]),
        Some("/tools") if tokens.len() == 1 => Outcome::Inspector(Some(InspectorPage::Tools)),
        Some("/tools") => Outcome::Notices(vec!["usage: /tools (no arguments)".to_owned()]),
        Some("/mcp") if tokens.len() == 1 => Outcome::Inspector(Some(InspectorPage::Mcp)),
        Some("/mcp") => Outcome::Notices(vec!["usage: /mcp (no arguments)".to_owned()]),
        Some("/hooks") if tokens.len() == 1 => Outcome::Inspector(Some(InspectorPage::Hooks)),
        Some("/hooks") => Outcome::Notices(vec!["usage: /hooks (no arguments)".to_owned()]),
        Some("/panel") if tokens.len() == 1 => Outcome::Inspector(None),
        Some("/panel") => Outcome::Notices(vec!["usage: /panel (no arguments)".to_owned()]),
        Some(other) if other.starts_with("/skill:") => skill_command(other, line),
        Some(other) => Outcome::Notices(vec![format!("unknown command {other}; /help lists them")]),
        None => Outcome::Message,
    }
}

/// `/jobs` with no arguments (ADR 0037 decision 4): one notice line per **running**
/// background job, in registry order; `no running background jobs` when none is running.
/// Finished background jobs and settled foreground retentions (decision 3a) stay out of the
/// list but remain readable through the `job` tool. The loop calls this with the facade's
/// `jobs()`; `Jobs::list` itself still returns every entry for its other caller.
pub fn jobs_list_notices(jobs: &[radian_coding::JobSummary]) -> Vec<String> {
    let lines: Vec<String> = jobs
        .iter()
        .filter(|job| job.state.is_running())
        .map(|job| {
            format!(
                "{}  {}  {}s  {}  {}",
                job.id,
                job.state.label(),
                job.elapsed.as_secs(),
                job.command,
                job.output
            )
        })
        .collect();
    if lines.is_empty() {
        vec!["no running background jobs".to_owned()]
    } else {
        lines
    }
}

/// `/jobs [kill <id|all>]` (ADR 0037 decision 4, inspector panel): a bare `/jobs` opens
/// the inspector's jobs page; `kill` forms stay text notices on the same path as before,
/// so scripts and herdr are unaffected. Anything else is a usage notice.
fn jobs_request(args: &[String]) -> Outcome {
    match args {
        [] => Outcome::Inspector(Some(InspectorPage::Jobs)),
        [kill, id] if kill == "kill" && id == "all" => Outcome::Jobs(JobsRequest::KillAll),
        [kill, id] if kill == "kill" && !id.is_empty() => {
            Outcome::Jobs(JobsRequest::Kill(id.clone()))
        }
        _ => Outcome::Notices(vec!["usage: /jobs [kill <id|all>]".to_owned()]),
    }
}

/// `/theme <dark|light|mono|name>`: exactly one name (ADR 0008 revision). Anything else is
/// a usage notice.
fn theme_name(args: &[String]) -> Outcome {
    match (args.first(), args.get(1)) {
        (Some(name), None) => Outcome::Theme(name.clone()),
        _ => Outcome::Notices(vec![
            "usage: /theme <dark|light|mono|theme-file-name>".to_owned(),
        ]),
    }
}

/// `/login [provider]`: `custom` or a built-in id skips the choice step; anything else is a
/// usage notice. More than one name is the same error.
fn login_provider(args: &[String]) -> Option<String> {
    match args {
        [] => None,
        [one] => {
            let valid = one == crate::login::CUSTOM
                || one == radian_host::CHATGPT_PROVIDER_ID
                || one == radian_host::CHATGPT_DEVICE_LOGIN
                || radian_host::descriptor(one).is_some();
            if valid {
                Some(one.clone())
            } else {
                // Reported as a notice by the caller (an `Option` cannot carry the reason).
                Some(format!("no-such-provider:{one}"))
            }
        }
        [_, extra, ..] => Some(format!("no-such-provider:{extra}")),
    }
}

/// `/logout <provider>`: exactly one name; empty is a usage notice the caller renders.
fn logout_provider(args: &[String]) -> String {
    match args {
        [one] => one.clone(),
        [] => String::new(),
        [_, extra, ..] => format!("__extra__{extra}"),
    }
}

/// `/usage`'s notice lines (ADR 0027): every window of the current
/// provider's snapshot, one line each, then the provider's own trailing word. This is the full
/// form — the footer's single line is the short one, and the two are deliberately different
/// views of the same snapshot.
pub fn usage_notices(snapshot: &radian_host::UsageSnapshot, stale: bool) -> Vec<String> {
    let mut header = format!("{} ({})", snapshot.title, snapshot.provider);
    if stale {
        header.push_str(" (stale)");
    }
    let mut out = vec![header];
    for window in &snapshot.windows {
        let mut parts = Vec::new();
        if let Some(remaining) = &window.remaining {
            parts.push(format!("{} remaining", remaining.text()));
        }
        if let Some(reset) = &window.resets_at {
            parts.push(format!("resets in {reset}"));
        }
        out.push(format!("{}: {}", window.label, parts.join(", ")));
    }
    if let Some(plan) = &snapshot.plan {
        out.push(format!("plan: {plan}"));
    }
    out
}

/// The optional `/compact` note: the arguments joined with spaces, or `None` when empty.
fn compact_instructions(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let joined = joined.trim();
    (!joined.is_empty()).then(|| joined.to_owned())
}

/// `/auto on|off` (and `/yolo`, its alias). Anything else is a usage notice (ADR 0026 D9).
fn auto_toggle(args: &[String]) -> Outcome {
    match args.first().map(String::as_str) {
        Some("on") => Outcome::AutoToggle(true),
        Some("off") => Outcome::AutoToggle(false),
        _ => Outcome::Notices(vec![
            "usage: /auto on|off (deny rules and the sandbox still apply)".to_owned(),
        ]),
    }
}

/// `/access [workspace|full]` (ADR 0038 slice 2). No argument is the query; one name is
/// the switch; anything else is a usage notice.
fn access_request(args: &[String]) -> Outcome {
    match (args.first(), args.get(1)) {
        (None, _) => Outcome::Access(None),
        (Some(name), None) => Outcome::Access(Some(name.clone())),
        _ => Outcome::Notices(vec!["usage: /access [workspace|full]".to_owned()]),
    }
}

/// The optional `/sessions` query: the arguments joined with spaces, or `None` when empty.
fn sessions_query(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let joined = joined.trim();
    (!joined.is_empty()).then(|| joined.to_owned())
}

/// `/export [md|json] [path] [--force]`. The format defaults to Markdown, the path to
/// `<workspace>/radian-<id>.<ext>`. An unrecognized token is the path; `--force` allows
/// overwriting. More than one path is a usage error the user sees as a notice.
fn export_request(args: &[String]) -> Outcome {
    let mut format = radian_host::Format::Markdown;
    let mut path: Option<String> = None;
    let mut force = false;
    for arg in args {
        match arg.as_str() {
            "md" | "markdown" => format = radian_host::Format::Markdown,
            "json" => format = radian_host::Format::Json,
            "--force" => force = true,
            other if path.is_none() => path = Some(other.to_owned()),
            other => {
                return Outcome::Notices(vec![format!(
                    "unexpected extra path {other:?}; usage: /export [md|json] [path] [--force]"
                )]);
            }
        }
    }
    Outcome::Export(ExportRequest {
        format,
        path,
        force,
    })
}

/// `/queue <text>`: the arguments joined with spaces, like `/compact`'s note. Empty is a
/// usage notice.
fn queue_text(args: &[String]) -> Outcome {
    let joined = args.join(" ");
    let joined = joined.trim();
    if joined.is_empty() {
        return Outcome::Notices(vec!["usage: /queue <text>".to_owned()]);
    }
    Outcome::Queue(joined.to_owned())
}

/// What `/loop` asked for: a bare toggle, or an enable with limits and an optional prompt.
/// A bare `/loop` toggles (off when on; armed when off — the next message becomes the loop
/// prompt). Flags come before the prompt; a `--` token after the prompt started is a usage
/// error, never silent prompt text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopCommand {
    /// A bare `/loop`: the app toggles the mode.
    Toggle,
    /// `/loop [--every <dur>] [--times N] [--for <dur>] [prompt]`.
    Enable(LoopOptions),
}

/// The `/loop` limits and prompt. Durations are `std::time::Duration`; `None` is "not
/// given" (the app resolves the default cap).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoopOptions {
    /// Wait at least this long between iteration starts (`--every <dur>`).
    pub every: Option<std::time::Duration>,
    /// Stop after this many loop turns (`--times N`).
    pub times: Option<u64>,
    /// Stop this long after the loop started (`--for <dur>`).
    pub for_duration: Option<std::time::Duration>,
    /// The loop prompt (`None` = armed: the next user message becomes it).
    pub prompt: Option<String>,
}

/// The `/loop` usage line, shared by every refusal below.
const LOOP_USAGE: &str = "usage: /loop [--every <dur>] [--times N] [--for <dur>] [prompt] \
    (durations like 30s, 5m, 2h; flags come before the prompt; a bare number or duration \
    needs its flag; a bare /loop toggles)";

/// `/loop ...`: flags (`--every`, `--times`, `--for`) before the prompt, then the prompt
/// words. A bare number or a bare duration is refused even as a prompt (omp's limit versus
/// pi-loop's interval would be ambiguous); anything else unknown is the same usage notice.
fn loop_request(args: &[String]) -> Outcome {
    if args.is_empty() {
        return Outcome::Loop(LoopCommand::Toggle);
    }
    let mut options = LoopOptions::default();
    let mut prompt_at: Option<usize> = None;
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        if prompt_at.is_none() && token.starts_with("--") {
            let flag = token.trim_start_matches('-');
            match flag {
                "every" | "times" | "for" => {
                    let Some(value) = args.get(index + 1) else {
                        return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
                    };
                    match flag {
                        "every" => {
                            let Some(duration) = parse_loop_duration(value) else {
                                return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
                            };
                            options.every = Some(duration);
                        }
                        "times" => {
                            let Ok(times) = value.parse::<u64>() else {
                                return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
                            };
                            if times == 0 {
                                return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
                            }
                            options.times = Some(times);
                        }
                        _ => {
                            let Some(duration) = parse_loop_duration(value) else {
                                return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
                            };
                            options.for_duration = Some(duration);
                        }
                    }
                    index += 2;
                }
                _ => return Outcome::Notices(vec![LOOP_USAGE.to_owned()]),
            }
        } else if token.starts_with("--") {
            // A flag after the prompt started: refuse rather than loop the literal flag
            // text ("flags come before the prompt" is the rule the usage line states).
            return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
        } else {
            if prompt_at.is_none() {
                prompt_at = Some(index);
            }
            index += 1;
        }
    }
    if let Some(start) = prompt_at {
        let prompt = args[start..].join(" ");
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
        }
        // One bare number or duration is not a prompt: it needs its flag.
        if !prompt.contains(' ')
            && (prompt.parse::<u64>().is_ok() || parse_loop_duration(prompt).is_some())
        {
            return Outcome::Notices(vec![LOOP_USAGE.to_owned()]);
        }
        options.prompt = Some(prompt.to_owned());
    }
    Outcome::Loop(LoopCommand::Enable(options))
}

/// A loop duration: a positive integer with one unit (`30s`, `5m`, `2h`). Anything else —
/// no unit, an unknown unit, zero, overflow — is `None`. The bare-number refusal above
/// reuses it, so the two stay one definition.
fn parse_loop_duration(text: &str) -> Option<std::time::Duration> {
    let unit = text.chars().last()?;
    if !matches!(unit, 's' | 'm' | 'h') {
        return None;
    }
    let number: u64 = text[..text.len() - unit.len_utf8()].parse().ok()?;
    if number == 0 {
        return None;
    }
    let secs = match unit {
        's' => number,
        'm' => number.checked_mul(60)?,
        _ => number.checked_mul(3600)?,
    };
    Some(std::time::Duration::from_secs(secs))
}

/// What `/goal` asked for (ADR 0047). Parsing only: running it journals through the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCommand {
    /// A bare `/goal`: show the goal, its progress and its budget use.
    Show,
    /// `/goal <objective>`: set or replace the goal (at most [`GOAL_OBJECTIVE_LIMIT`]
    /// characters); the budget resets to the default.
    Set { objective: String },
    /// `/goal pause`: stop continuing, keep the goal for `/goal resume`.
    Pause,
    /// `/goal resume`: continue a paused goal (or restart its stretch after `--resume`).
    Resume,
    /// `/goal drop`: forget the goal entirely.
    Drop,
    /// `/goal budget …`: set caps, one or more of `<N> turns`, `<dur>`, `<N> tokens`.
    Budget { specs: Vec<GoalBudgetSpec> },
}

/// One `/goal budget` cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalBudgetSpec {
    Turns(u64),
    Duration(std::time::Duration),
    Tokens(u64),
}

/// The longest objective `/goal` accepts (Codex's `/goal` limit, ADR 0047).
pub const GOAL_OBJECTIVE_LIMIT: usize = 4000;

/// The `/goal` usage line, shared by every refusal below.
const GOAL_USAGE: &str = "usage: /goal <objective> (up to 4000 chars) | /goal (show) | \
    /goal pause | /goal resume | /goal drop | /goal budget [<N> turns] [<dur>] [<N> tokens] \
    (durations like 30s, 5m, 2h)";

/// `/goal …`: the subcommand word decides; anything after `pause`/`resume`/`drop` is a
/// usage error, never silent prompt text. An objective that is one bare number or duration
/// is refused (like `/loop`'s prompt: it needs its budget flag to mean anything).
fn goal_request(args: &[String]) -> Outcome {
    if args.is_empty() {
        return Outcome::Goal(GoalCommand::Show);
    }
    match args[0].as_str() {
        "pause" if args.len() == 1 => Outcome::Goal(GoalCommand::Pause),
        "resume" if args.len() == 1 => Outcome::Goal(GoalCommand::Resume),
        "drop" if args.len() == 1 => Outcome::Goal(GoalCommand::Drop),
        "budget" => goal_budget_request(&args[1..]),
        "pause" | "resume" | "drop" => Outcome::Notices(vec![GOAL_USAGE.to_owned()]),
        _ => goal_set_request(args),
    }
}

/// `/goal <objective>`: the arguments joined with spaces, like `/queue`'s text.
fn goal_set_request(args: &[String]) -> Outcome {
    let objective = args.join(" ");
    let objective = objective.trim();
    if objective.is_empty() {
        return Outcome::Notices(vec![GOAL_USAGE.to_owned()]);
    }
    // One bare number or duration is not an objective: it needs its budget flag.
    if !objective.contains(' ')
        && (objective.parse::<u64>().is_ok() || parse_loop_duration(objective).is_some())
    {
        return Outcome::Notices(vec![GOAL_USAGE.to_owned()]);
    }
    if objective.chars().count() > GOAL_OBJECTIVE_LIMIT {
        return Outcome::Notices(vec![format!(
            "the objective is {} characters (the limit is {GOAL_OBJECTIVE_LIMIT}); shorten it",
            objective.chars().count()
        )]);
    }
    Outcome::Goal(GoalCommand::Set {
        objective: objective.to_owned(),
    })
}

/// `/goal budget …`: one or more `<N> turns|turn`, `<dur>`, `<N> tokens|token` pairs. Bare
/// (no pairs) shows the budget instead of changing it.
fn goal_budget_request(args: &[String]) -> Outcome {
    if args.is_empty() {
        return Outcome::Goal(GoalCommand::Budget { specs: Vec::new() });
    }
    let mut specs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let (spec, next) = match goal_budget_pair(args, index) {
            Some(pair) => pair,
            None => return Outcome::Notices(vec![GOAL_USAGE.to_owned()]),
        };
        specs.push(spec);
        index = next;
    }
    Outcome::Goal(GoalCommand::Budget { specs })
}

/// One budget pair at `args[index]`: `<N> turns|turn`, a duration, or `<N> tokens|token`.
/// Returns the spec and the next index.
fn goal_budget_pair(args: &[String], index: usize) -> Option<(GoalBudgetSpec, usize)> {
    // A duration stands alone (`2h`); anything else starts with a number.
    if let Some(duration) = parse_loop_duration(&args[index]) {
        return Some((GoalBudgetSpec::Duration(duration), index + 1));
    }
    let number: u64 = args[index].parse().ok()?;
    if number == 0 {
        return None;
    }
    let unit = args.get(index + 1)?.as_str();
    match unit {
        "turn" | "turns" => Some((GoalBudgetSpec::Turns(number), index + 2)),
        "token" | "tokens" => Some((GoalBudgetSpec::Tokens(number), index + 2)),
        _ => None,
    }
}

/// `/goal`'s one completion level: the subcommand words, then free text.
fn goal_level(typed: &[&str], _partial: &str) -> Level {
    match typed.first() {
        None => Level::Ready(vec![
            Candidate {
                insert: "pause".to_owned(),
                description: "stop continuing, keep the goal".to_owned(),
                current: false,
            },
            Candidate {
                insert: "resume".to_owned(),
                description: "continue a paused goal".to_owned(),
                current: false,
            },
            Candidate {
                insert: "drop".to_owned(),
                description: "forget the goal".to_owned(),
                current: false,
            },
            Candidate {
                insert: "budget".to_owned(),
                description: "cap the goal: turns, duration, tokens".to_owned(),
                current: false,
            },
        ]),
        Some(_) => Level::Free,
    }
}

/// `/copy [code]`: no argument copies the whole message, `code` only its last fenced block.
/// Anything else is a usage notice.
fn copy_request(args: &[String]) -> Outcome {
    match args {
        [] => Outcome::Copy { code_only: false },
        [one] if one == "code" => Outcome::Copy { code_only: true },
        _ => Outcome::Notices(vec!["usage: /copy [code]".to_owned()]),
    }
}

fn memory_command(args: &[String], memory_enabled: bool) -> Outcome {
    if !memory_enabled {
        return Outcome::Notices(vec![
            "memory is disabled (no RADIAN_HOME or HOME)".to_owned(),
        ]);
    }
    match commands::parse_args(args) {
        Ok(command) => Outcome::Memory(command),
        // clap's own error/help text is shown as a notice; the program does not exit.
        Err(error) => Outcome::Notices(vec![error.to_string().trim_end().to_owned()]),
    }
}

/// The lines one command output becomes: the diagnostics note, then the text's lines. An
/// output with neither becomes one `(no output)` line.
pub fn output_notices(output: &CommandOutput) -> Vec<String> {
    let mut notices = Vec::new();
    if let Some(note) = &output.note {
        notices.push(note.clone());
    }
    notices.extend(output.text.lines().map(str::to_owned));
    if notices.is_empty() {
        notices.push("(no output)".to_owned());
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0033: a leading `!` is a shell command, before the `/…` commands. `!!` keeps the
    /// output out of the model's context; an empty command is a one-line usage notice.
    #[test]
    fn a_leading_bang_is_a_shell_command() {
        assert!(matches!(
            parse("!echo hi", false),
            Outcome::Bash { command, exclude_from_context: false } if command == "echo hi"
        ));
        assert!(matches!(
            parse("!!echo hi", false),
            Outcome::Bash { command, exclude_from_context: true } if command == "echo hi"
        ));
        // `!/bin/sh` is the command `/bin/sh`, not the unknown `/bin/sh` command.
        assert!(matches!(
            parse("!/bin/echo x", false),
            Outcome::Bash { command, .. } if command == "/bin/echo x"
        ));
        // The shell does its own quoting: the line is not shlexed here.
        assert!(matches!(
            parse("!echo 'a b' | grep a", false),
            Outcome::Bash { command, .. } if command == "echo 'a b' | grep a"
        ));
        for empty in ["!", "!!", "!   "] {
            match parse(empty, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: !"), "{notices:?}")
                }
                other => panic!("expected usage for {empty:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn help_and_unknown_commands_are_notices() {
        match parse("/help", false) {
            Outcome::Notices(notices) => assert!(notices[0].contains("/memory"), "{notices:?}"),
            other => panic!("expected notices, got {other:?}"),
        }
        match parse("/nope", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("unknown command /nope"), "{notices:?}")
            }
            other => panic!("expected notices, got {other:?}"),
        }
        match parse("/memory add \"unbalanced", true) {
            Outcome::Notices(notices) => assert!(notices[0].contains("unbalanced quotes")),
            other => panic!("expected a quote error, got {other:?}"),
        }
    }

    /// Parsing only: the command comes back untouched, and whether it needs confirmation is
    /// the executor's answer (ADR 0023 U3), not this layer's.
    #[test]
    fn a_memory_write_parses_to_a_command_without_running_it() {
        match parse("/memory forget abc", true) {
            Outcome::Memory(command) => {
                let debug = format!("{command:?}");
                assert!(debug.contains("Forget"), "{debug}");
                assert!(debug.contains("abc"), "{debug}");
            }
            other => panic!("expected a memory command, got {other:?}"),
        }
        // A command that does not parse is a notice, not a panic and not an exit.
        match parse("/memory forget", true) {
            Outcome::Notices(notices) => {
                assert!(!notices.is_empty());
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
    }

    #[test]
    fn sessions_and_export_parse() {
        assert!(matches!(parse("/sessions", false), Outcome::Sessions(None)));
        match parse("/sessions parser bug", false) {
            Outcome::Sessions(Some(query)) => assert_eq!(query, "parser bug"),
            other => panic!("expected a sessions query, got {other:?}"),
        }
        match parse("/export", false) {
            Outcome::Export(request) => {
                assert_eq!(request.format, radian_host::Format::Markdown);
                assert!(request.path.is_none());
                assert!(!request.force);
            }
            other => panic!("expected an export request, got {other:?}"),
        }
        match parse("/export json out.json --force", false) {
            Outcome::Export(request) => {
                assert_eq!(request.format, radian_host::Format::Json);
                assert_eq!(request.path.as_deref(), Some("out.json"));
                assert!(request.force);
            }
            other => panic!("expected an export request, got {other:?}"),
        }
        // A bare path is the path, not the format.
        match parse("/export result.txt", false) {
            Outcome::Export(request) => {
                assert_eq!(request.format, radian_host::Format::Markdown);
                assert_eq!(request.path.as_deref(), Some("result.txt"));
            }
            other => panic!("expected an export request, got {other:?}"),
        }
        match parse("/export a b", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("unexpected extra path"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        assert!(
            HELP.contains("/sessions") && HELP.contains("/export"),
            "{HELP}"
        );
    }

    #[test]
    fn compact_is_a_control_request_with_optional_instructions() {
        assert!(matches!(parse("/compact", false), Outcome::Compact(None)));
        match parse("/compact focus on the parser", false) {
            Outcome::Compact(Some(note)) => assert_eq!(note, "focus on the parser"),
            other => panic!("expected a compact request, got {other:?}"),
        }
    }

    /// `/queue <text>` parses to the queued text (arguments joined, like `/compact`'s note);
    /// empty is a usage notice. It is a command, and it is in `/help`.
    #[test]
    fn queue_parses_to_its_text_and_rejects_an_empty_line() {
        match parse("/queue do this next", false) {
            Outcome::Queue(text) => assert_eq!(text, "do this next"),
            other => panic!("expected the queued text, got {other:?}"),
        }
        match parse("/queue \"quoted  words\"", false) {
            Outcome::Queue(text) => assert_eq!(text, "quoted  words"),
            other => panic!("expected the queued text, got {other:?}"),
        }
        for line in ["/queue", "/queue   "] {
            match parse(line, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: /queue"), "{line}: {notices:?}")
                }
                other => panic!("{line}: expected notices, got {other:?}"),
            }
        }
        assert!(HELP.contains("/queue"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "queue"));
    }

    /// `/new` takes no argument; anything after it is a usage notice. It is a command,
    /// and it is in `/help`.
    #[test]
    fn new_takes_no_arguments() {
        assert!(matches!(parse("/new", false), Outcome::New));
        match parse("/new now", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /new"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        assert!(HELP.contains("/new"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "new"));
    }

    /// `/retry` takes no argument; anything after it is a usage notice. It is a command,
    /// and it is in `/help`.
    #[test]
    fn retry_takes_no_arguments() {
        assert!(matches!(parse("/retry", false), Outcome::Retry));
        match parse("/retry now", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /retry"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        assert!(HELP.contains("/retry"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "retry"));
    }

    /// `/loop`: a bare line toggles; flags (`--every`, `--times`, `--for`) come before the
    /// prompt; a bare number or bare duration is refused even as a prompt; a flag after the
    /// prompt started, an unknown flag, a missing value and a zero limit are usage notices.
    /// It is a command, and it is in `/help`.
    #[test]
    fn loop_parses_toggle_flags_and_prompt() {
        use std::time::Duration;

        assert!(matches!(
            parse("/loop", false),
            Outcome::Loop(LoopCommand::Toggle)
        ));

        // Flags with a prompt.
        match parse("/loop --every 5m --times 3 fix the build", false) {
            Outcome::Loop(LoopCommand::Enable(options)) => {
                assert_eq!(options.every, Some(Duration::from_secs(300)));
                assert_eq!(options.times, Some(3));
                assert_eq!(options.for_duration, None);
                assert_eq!(options.prompt.as_deref(), Some("fix the build"));
            }
            other => panic!("expected the loop options, got {other:?}"),
        }
        // Flags without a prompt arm the loop; the next message becomes the prompt.
        match parse("/loop --times 2", false) {
            Outcome::Loop(LoopCommand::Enable(options)) => {
                assert_eq!(options.times, Some(2));
                assert_eq!(options.prompt, None);
            }
            other => panic!("expected an armed loop, got {other:?}"),
        }
        // A prompt with no flags.
        match parse("/loop watch the tests", false) {
            Outcome::Loop(LoopCommand::Enable(options)) => {
                assert_eq!(options.prompt.as_deref(), Some("watch the tests"));
                assert_eq!(options.every, None);
            }
            other => panic!("expected a prompt-only loop, got {other:?}"),
        }
        // A multi-word prompt may start with a number; a single bare number or bare
        // duration is refused (it needs its flag).
        match parse("/loop run 5 times", false) {
            Outcome::Loop(LoopCommand::Enable(options)) => {
                assert_eq!(options.prompt.as_deref(), Some("run 5 times"));
            }
            other => panic!("expected the prompt, got {other:?}"),
        }
        for line in [
            "/loop 5",
            "/loop 5m",
            "/loop 0",
            "/loop --times 0",
            "/loop --every",
        ] {
            match parse(line, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: /loop"), "{line}: {notices:?}")
                }
                other => panic!("{line}: expected a usage notice, got {other:?}"),
            }
        }
        // A flag after the prompt started, an unknown flag and a bad duration are the
        // same usage notice — never silent prompt text.
        for line in [
            "/loop fix it --times 2",
            "/loop --forever 2 fix it",
            "/loop --every 5x fix it",
            "/loop --for two fix it",
        ] {
            match parse(line, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: /loop"), "{line}: {notices:?}")
                }
                other => panic!("{line}: expected a usage notice, got {other:?}"),
            }
        }
        // Durations take seconds, minutes and hours; zero is refused.
        match parse("/loop --every 30s --for 2h go", false) {
            Outcome::Loop(LoopCommand::Enable(options)) => {
                assert_eq!(options.every, Some(Duration::from_secs(30)));
                assert_eq!(options.for_duration, Some(Duration::from_secs(7200)));
            }
            other => panic!("expected durations, got {other:?}"),
        }
        assert!(HELP.contains("/loop"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "loop"));
    }

    /// `/goal`: bare shows, an objective sets, `pause`/`resume`/`drop` control, `budget`
    /// caps with turns/duration/tokens pairs. Refusals (bare numbers, bad pairs, extra words,
    /// an over-long objective) are usage notices. It is a command, and it is in `/help`.
    #[test]
    fn goal_parses_show_set_control_and_budget() {
        use std::time::Duration;

        assert!(matches!(
            parse("/goal", false),
            Outcome::Goal(GoalCommand::Show)
        ));
        match parse("/goal ship the refactor", false) {
            Outcome::Goal(GoalCommand::Set { objective }) => {
                assert_eq!(objective, "ship the refactor")
            }
            other => panic!("expected a set, got {other:?}"),
        }
        assert!(matches!(
            parse("/goal pause", false),
            Outcome::Goal(GoalCommand::Pause)
        ));
        assert!(matches!(
            parse("/goal resume", false),
            Outcome::Goal(GoalCommand::Resume)
        ));
        assert!(matches!(
            parse("/goal drop", false),
            Outcome::Goal(GoalCommand::Drop)
        ));
        // A bare number or duration is not an objective (it needs its budget flag).
        for line in ["/goal 5", "/goal 5m"] {
            match parse(line, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: /goal"), "{line}: {notices:?}")
                }
                other => panic!("{line}: expected a usage notice, got {other:?}"),
            }
        }
        // Budget pairs, alone and combined; a bare `budget` shows.
        match parse("/goal budget 2 turns", false) {
            Outcome::Goal(GoalCommand::Budget { specs }) => {
                assert_eq!(specs, vec![GoalBudgetSpec::Turns(2)])
            }
            other => panic!("expected a budget, got {other:?}"),
        }
        match parse("/goal budget 10 turns 2h 50000 tokens", false) {
            Outcome::Goal(GoalCommand::Budget { specs }) => {
                assert_eq!(
                    specs,
                    vec![
                        GoalBudgetSpec::Turns(10),
                        GoalBudgetSpec::Duration(Duration::from_secs(7200)),
                        GoalBudgetSpec::Tokens(50000),
                    ]
                )
            }
            other => panic!("expected a combined budget, got {other:?}"),
        }
        assert!(matches!(
            parse("/goal budget", false),
            Outcome::Goal(GoalCommand::Budget { specs }) if specs.is_empty()
        ));
        // Bad pairs, zero caps and extra words after the verbs are usage notices.
        for line in [
            "/goal budget 2",
            "/goal budget 0 turns",
            "/goal budget 5x",
            "/goal budget turns 2",
            "/goal pause now",
            "/goal resume later",
            "/goal drop it",
        ] {
            match parse(line, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: /goal"), "{line}: {notices:?}")
                }
                other => panic!("{line}: expected a usage notice, got {other:?}"),
            }
        }
        // The objective limit is enforced at parse time.
        match parse(
            &format!("/goal {}", "x".repeat(GOAL_OBJECTIVE_LIMIT + 1)),
            false,
        ) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("the limit is"), "{notices:?}")
            }
            other => panic!("expected a limit notice, got {other:?}"),
        }
        assert!(HELP.contains("/goal"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "goal"));
    }

    /// `/copy` copies the whole message, `/copy code` only its last block; anything else is
    /// a usage notice. It is a command, and it is in `/help`.
    #[test]
    fn copy_parses_its_optional_code_selector() {
        assert!(matches!(
            parse("/copy", false),
            Outcome::Copy { code_only: false }
        ));
        assert!(matches!(
            parse("/copy code", false),
            Outcome::Copy { code_only: true }
        ));
        match parse("/copy everything please", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /copy"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        assert!(HELP.contains("/copy"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "copy"));
    }

    /// The completion table and the clap grammar must name the same subcommands, in both
    /// directions: the table is what the UI completes, the grammar is what runs.
    #[test]
    fn the_memory_subcommand_table_matches_the_grammar() {
        let grammar: Vec<String> =
            <MemoryCommand as clap::Subcommand>::augment_subcommands(clap::Command::new("memory"))
                .get_subcommands()
                .map(|sub| sub.get_name().to_owned())
                .collect();
        for (name, _) in MEMORY_SUBCOMMANDS {
            assert!(
                grammar.iter().any(|known| known == name),
                "`{name}` is in the completion table but not in the grammar: {grammar:?}"
            );
        }
        for name in &grammar {
            assert!(
                MEMORY_SUBCOMMANDS.iter().any(|(known, _)| known == name),
                "`{name}` is in the grammar but not in the completion table"
            );
        }
    }

    #[test]
    fn every_command_description_comes_from_help() {
        // `/help` wraps its long lines, so compare on collapsed whitespace.
        let help = HELP.split_whitespace().collect::<Vec<_>>().join(" ");
        for command in tree() {
            let description = command
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                help.contains(&description),
                "`{}`'s description is not in /help: {description}",
                command.name
            );
        }
    }

    /// ADR 0025 S3: `/skill:<name> [args]` parses to the skill command with the raw remainder as
    /// verbatim args (quotes survive); an empty name and `/skills` with arguments are notices.
    #[test]
    fn skill_commands_parse() {
        match parse("/skill:greet hello \"big world\"", false) {
            Outcome::Skill { name, args } => {
                assert_eq!(name, "greet");
                assert_eq!(args, "hello \"big world\"");
            }
            other => panic!("expected the skill command, got {other:?}"),
        }
        match parse("/skill:greet", false) {
            Outcome::Skill { name, args } => {
                assert_eq!(name, "greet");
                assert!(args.is_empty(), "{args:?}");
            }
            other => panic!("expected the skill command, got {other:?}"),
        }
        match parse("/skill:", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /skill:"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        match parse("/skills", false) {
            Outcome::Skills => {}
            other => panic!("expected /skills, got {other:?}"),
        }
        match parse("/skills now", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /skills"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
    }

    /// ADR 0027: `/usage` takes no argument, is a command, and is in
    /// `/help`; anything after it is a usage notice.
    #[test]
    fn usage_is_a_command_with_no_arguments() {
        assert!(matches!(parse("/usage", false), Outcome::Usage));
        match parse("/usage now", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /usage"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        assert!(HELP.contains("/usage"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "usage"));
        assert!(tree().iter().any(|command| command.name == "context"));
    }

    /// `/context` takes no argument, is a command, and is in `/help`; anything
    /// after it is a usage notice.
    #[test]
    fn context_is_a_command_with_no_arguments() {
        assert!(matches!(parse("/context", false), Outcome::Context));
        match parse("/context now", false) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("usage: /context"), "{notices:?}")
            }
            other => panic!("expected a usage notice, got {other:?}"),
        }
        assert!(HELP.contains("/context"), "{HELP}");
        assert!(tree().iter().any(|command| command.name == "context"));
    }

    /// `/usage`'s lines list **every** window, one line each, then the provider's trailing word
    /// (ADR 0027: the footer's one line is the short form).
    #[test]
    fn usage_notices_lists_every_window() {
        let snapshot = radian_host::UsageSnapshot {
            provider: "chatgpt".to_owned(),
            title: "ChatGPT usage".to_owned(),
            windows: vec![
                radian_host::UsageWindow {
                    label: "5h".to_owned(),
                    remaining: Some(radian_host::Remaining::Percent(95.0)),
                    resets_at: Some("1h0m".to_owned()),
                },
                radian_host::UsageWindow {
                    label: "7d".to_owned(),
                    remaining: Some(radian_host::Remaining::Percent(60.0)),
                    resets_at: None,
                },
            ],
            plan: Some("pro".to_owned()),
        };
        assert_eq!(
            usage_notices(&snapshot, false),
            vec![
                "ChatGPT usage (chatgpt)".to_owned(),
                "5h: 95% remaining, resets in 1h0m".to_owned(),
                "7d: 60% remaining".to_owned(),
                "plan: pro".to_owned(),
            ]
        );
        assert!(
            usage_notices(&snapshot, true)[0].contains("(stale)"),
            "a stale snapshot says so"
        );
    }

    /// ADR 0025 S3 / ruling 7: `/skill:` completes the discovered names, which only the host
    /// knows, so the completion is a `Needs(SkillNames)` fetch with the name's own range.
    #[test]
    fn the_skill_command_completes_the_name_through_the_host() {
        let line = "/skill:gr";
        let completion = complete_in(tree(), line, line.chars().count()).expect("a completion");
        assert_eq!(completion.level, 1);
        assert_eq!(completion.range, 7..9, "the range covers the partial name");
        assert_eq!(
            completion.level_result,
            Level::Needs(Fetch::SkillNames {
                partial: "gr".into()
            })
        );

        // `/skill:` with nothing after the colon lists every name.
        let line = "/skill:";
        let completion = complete_in(tree(), line, line.chars().count()).expect("a completion");
        assert_eq!(completion.range, 7..7);
        assert_eq!(
            completion.level_result,
            Level::Needs(Fetch::SkillNames {
                partial: String::new()
            })
        );

        // The command is in the tree, so a bare `/` offers it.
        assert!(tree().iter().any(|command| command.name == "skill:"));
        assert!(tree().iter().any(|command| command.name == "skills"));
        assert!(HELP.contains("/skill:<name>"));
        assert!(HELP.contains("/skills"));
    }

    #[test]
    fn fuzzy_matching_is_a_subsequence_with_prefixes_first() {
        // `/cmp` finds `/compact` (c-m-p in order) ...
        assert!(fuzzy_score("/compact", "cmp").is_some());
        assert!(fuzzy_score("/memory", "cmp").is_none(), "not a subsequence");
        // ... and a prefix beats a scattered match.
        let prefix = fuzzy_score("/compact", "co").unwrap();
        let scattered = fuzzy_score("/compact", "cpt").unwrap();
        assert!(prefix > scattered, "{prefix} vs {scattered}");
        // A leading slash is ignored on both sides.
        assert_eq!(
            fuzzy_score("/compact", "co"),
            fuzzy_score("compact", "co"),
            "the slash is not part of the match"
        );

        let ranked = rank(
            vec![
                Candidate {
                    insert: "/memory".into(),
                    description: String::new(),
                    current: false,
                },
                Candidate {
                    insert: "/compact".into(),
                    description: String::new(),
                    current: false,
                },
            ],
            "co",
        );
        assert_eq!(ranked.len(), 1, "only /compact matches `co`");
        assert_eq!(ranked[0].insert, "/compact");
    }

    #[test]
    fn the_tree_completes_every_level() {
        // The command name, and the chain: `/mem fo` completes to `/memory forget` in one step.
        let completion = complete_in(tree(), "/mem fo", 7).expect("a completion");
        match &completion.level_result {
            Level::Ready(candidates) => {
                assert_eq!(candidates.len(), 1, "{candidates:?}");
                assert_eq!(candidates[0].insert, "/memory forget");
            }
            other => panic!("expected candidates, got {other:?}"),
        }
        assert_eq!(completion.range, 0..7);

        // A bare partial name completes just the name, leaving the arguments for the next Tab.
        let completion = complete_in(tree(), "/mem fo", 4).expect("a completion");
        assert_eq!(completion.range, 0..4);
        match &completion.level_result {
            Level::Ready(candidates) => assert_eq!(candidates[0].insert, "/memory"),
            other => panic!("expected candidates, got {other:?}"),
        }

        // Level 1: `/memory`'s subcommand, fuzzy (`/memory fo` → forget).
        let line = "/memory fo";
        let completion = complete_in(tree(), line, line.chars().count()).expect("a completion");
        assert_eq!(completion.level, 1);
        assert_eq!(completion.range, 8..10);
        match &completion.level_result {
            Level::Ready(candidates) => {
                assert_eq!(candidates.len(), 1, "{candidates:?}");
                assert_eq!(candidates[0].insert, "forget");
            }
            other => panic!("expected candidates, got {other:?}"),
        }

        // An empty partial after the space lists the whole level.
        let line = "/memory ";
        match complete_in(tree(), line, line.chars().count()).map(|c| c.level_result) {
            Some(Level::Ready(candidates)) => {
                assert_eq!(candidates.len(), MEMORY_SUBCOMMANDS.len())
            }
            other => panic!("expected the subcommand list, got {other:?}"),
        }

        // Level 2: the record id needs the host (an effect, never I/O in `update`).
        let line = "/memory forget ab";
        let completion = complete_in(tree(), line, line.chars().count()).expect("a completion");
        assert_eq!(completion.level, 2);
        assert_eq!(
            completion.level_result,
            Level::Needs(Fetch::MemoryRecords {
                command: "forget".into(),
                partial: "ab".into(),
            })
        );

        // `/export`'s first level is the format, its second offers `--force`.
        let line = "/export ";
        match complete_in(tree(), line, line.chars().count()).map(|c| c.level_result) {
            Some(Level::Ready(candidates)) => {
                assert_eq!(
                    candidates
                        .iter()
                        .map(|c| c.insert.as_str())
                        .collect::<Vec<_>>(),
                    vec!["md", "json"]
                );
            }
            other => panic!("expected the format list, got {other:?}"),
        }

        // An exact command name shows no list, so Enter runs it.
        assert!(complete_in(tree(), "/help", 5).is_none());
        // Free text has nothing to complete.
        assert!(complete_in(tree(), "/sessions par", 13).is_none());
        // A line that is not a command has no slash completion.
        assert!(complete_in(tree(), "hello", 5).is_none());
    }

    /// A later command (the owner's `/model`/`/login`) is one entry in the same tree, and its
    /// level sources are ordinary pure functions: this fake one proves the structure carries it.
    #[test]
    fn a_future_command_hangs_on_the_same_tree() {
        fn provider_level(_typed: &[&str], _partial: &str) -> Level {
            Level::Ready(vec![Candidate {
                insert: "anthropic".into(),
                description: "the Messages API".into(),
                current: false,
            }])
        }
        static FAKE: &[Command] = &[Command {
            name: "model",
            description: "pick the provider and model (ADR 0026)",
            levels: &[provider_level, provider_level],
        }];

        let line = "/model an";
        let completion = complete_in(FAKE, line, line.chars().count()).expect("a completion");
        assert_eq!(completion.level, 1);
        match &completion.level_result {
            Level::Ready(candidates) => {
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].insert, "anthropic");
            }
            other => panic!("expected candidates, got {other:?}"),
        }
    }

    #[test]
    fn memory_is_disabled_without_a_root() {
        match parse("/memory list", false) {
            Outcome::Notices(notices) => assert!(notices[0].contains("disabled")),
            other => panic!("expected a notice, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn theme_parses_one_name_and_rejects_the_rest() {
        match parse("/theme light", false) {
            Outcome::Theme(name) => assert_eq!(name, "light"),
            other => panic!("expected Theme, got {other:?}"),
        }
        for line in ["/theme", "/theme light dark"] {
            match parse(line, false) {
                Outcome::Notices(notices) => {
                    assert!(notices[0].contains("usage: /theme"), "{line}: {notices:?}")
                }
                other => panic!("{line}: expected notices, got {other:?}"),
            }
        }
    }

    /// The completion asks the host (the loop lists the directory), and the host's listing
    /// carries the built-ins plus the `*.json` stems.
    #[test]
    fn the_theme_level_needs_the_host_and_lists_files() {
        let level = theme_level(&[], "li");
        assert!(
            matches!(level, Level::Needs(Fetch::Themes { .. })),
            "{level:?}"
        );

        let dir = std::env::temp_dir().join(format!("radian-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kanagawa.json"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a theme").unwrap();
        let candidates = theme_candidates(Some(&dir));
        let names: Vec<&str> = candidates.iter().map(|c| c.insert.as_str()).collect();
        assert!(names.contains(&"dark") && names.contains(&"light") && names.contains(&"mono"));
        assert!(names.contains(&"kanagawa"), "{names:?}");
        assert!(
            !names.contains(&"notes"),
            "only *.toml/*.json is a theme: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

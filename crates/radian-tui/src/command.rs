//! The minimal slash-command framework (ADR 0021 D7; ADR 0009 keeps the TUI full-screen).
//!
//! A submitted line whose first non-space character is `/` is a command: it never reaches the
//! model, the transcript or the session journal. `//…` is the escape — one `/` is dropped and
//! the rest is sent as an ordinary message.
//!
//! [`parse`] only **parses**: running a command is an effect the event loop executes, so the
//! app stays pure (ADR 0023 U3). `/memory` runs the same `radian-host::memory` command set
//! `agent memory` uses (one clap grammar, one executor). Adding a command is one arm here.

use radian_host::memory::commands::{self, CommandOutput, MemoryCommand};

/// The `/help` text. Kept short; the full grammar is `radian_host::memory::commands`.
pub const HELP: &str = "\
/help                 list the commands
/compact [note]       summarize the history now and keep the recent part (ADR 0018)
/sessions [query]     list this workspace's recent sessions, or search their text
/fork                 fork this session at its last turn boundary (ADR 0022)
/model <provider>/<model>
                      switch the model at the next turn boundary (ADR 0026 D5)
/thinking <level>     set the thinking level (off…max); unsupported levels clamp (ADR 0026 D11)
/auto on|off          auto-approve tool asks for this session (ADR 0026 D9)
/yolo                 alias of /auto
/export [md|json] [path] [--force]
                      export this session (default md, default <workspace>/radian-<id>.<ext>)
/memory <command>     durable memory: list, search, show, add, edit, archive, restore,
                      forget, purge, confirm, export, import, project, relink, split, merge
/skill:<name> [args]  run a skill: read its body into the message (ADR 0025)
/skills               list the discovered skills
/login [provider]     store an API key in credentials/auth.json (ADR 0026 D3); custom
                      names a new endpoint (name, protocol, base URL, key)
/logout <provider>    remove a stored credential (asks first)
//…                   send a message that starts with `/`";

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

/// What one submitted `/…` line meant. Parsing only: running the command is the loop's job
/// (ADR 0023 U3), and whether a write needs confirmation is the executor's answer.
#[derive(Debug)]
pub enum Outcome {
    /// Show these lines as notices and keep the line out of the conversation.
    Notices(Vec<String>),
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
    /// `/model <provider>/<model>` (ADR 0026 D5): the loop resolves the credential and the
    /// new budget, then the app decides direct queue or confirm-first (D7).
    Model(ModelSwitch),
    /// `/thinking <level>` (ADR 0026 D11): the loop clamps it against the current route's map.
    Thinking(String),
    /// `/login [provider]`: open the credential wizard. `Some(id)` skips the provider
    /// choice (a built-in id or `custom`); `None` starts at the choice.
    Login(Option<String>),
    /// `/logout <provider>`: remove one stored credential, after a confirmation.
    Logout(String),
    /// `/auto on|off` (and `/yolo`, its alias): the in-session auto-approval switch
    /// (ADR 0026 D9). `true` = on.
    AutoToggle(bool),
    /// `/skill:<name> [args]` (ADR 0025 S3): the host reads the body and sends
    /// `<skill name=…>…</skill>` + `args` as one user message. `args` is the raw remainder,
    /// verbatim.
    Skill { name: String, args: String },
    /// `/skills`: list the discovered skills (name + description) as notices.
    Skills,
    /// Not a command after all (an empty token list); the caller sends the line.
    Message,
}

// ---------- the command tree and its per-level completion (ADR 0023 U4) ----------

/// One completion candidate: what to insert and the one-line description shown beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub insert: String,
    pub description: String,
}

/// What a level needs from the host when it cannot list its candidates by itself. `update` never
/// does I/O (ADR 0023 U3), so the loop resolves this and feeds the answer back as
/// [`crate::app::Input::CompletionsReady`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// `/memory <command> <partial>`: the record ids that command can act on.
    MemoryRecords { command: String, partial: String },
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

/// `/auto on|off` (and `/yolo`): the two words are the whole argument grammar (ADR 0026 D9).
fn auto_level(_typed: &[&str], _partial: &str) -> Level {
    Level::Ready(vec![
        Candidate {
            insert: "on".to_owned(),
            description: "approve tool asks without asking, this session".to_owned(),
        },
        Candidate {
            insert: "off".to_owned(),
            description: "ask again (back to on-ask)".to_owned(),
        },
    ])
}

/// `/export`: the format, then the path (with `--force` offered as a candidate).
fn export_level(typed: &[&str], _partial: &str) -> Level {
    match typed.first() {
        None => Level::Ready(vec![
            Candidate {
                insert: "md".to_owned(),
                description: "markdown: the readable transcript".to_owned(),
            },
            Candidate {
                insert: "json".to_owned(),
                description: "json: the event document".to_owned(),
            },
        ]),
        Some(_) => Level::Ready(vec![Candidate {
            insert: "--force".to_owned(),
            description: "overwrite an existing file".to_owned(),
        }]),
    }
}

/// A level that takes free text: nothing to complete.
fn free_level(_typed: &[&str], _partial: &str) -> Level {
    Level::Free
}

/// The completion for `line` at `cursor`: the char range a candidate replaces, and what that
/// level offers. Pure. `None` when the line is not a slash command, the command or the level is
/// unknown, the level is free text, or the only candidate is already exactly what was typed.
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
            if candidates.len() == 1 && candidates[0].insert == partial {
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

/// `/model`'s one argument level completes both halves of `<provider>/<model>`: the provider
/// before the slash, the model after it (ADR 0026 D5). The candidates need the host's
/// credential store and the model lists, so the level is `Needs` and the loop resolves it.
fn model_route_level(typed: &[&str], partial: &str) -> Level {
    let typed = typed.last().copied().unwrap_or_default();
    let word = if partial.is_empty() { typed } else { partial };
    let (provider, model_partial) = match word.split_once('/') {
        Some((provider, model)) => (Some(provider.to_owned()), model.to_owned()),
        None => (None, String::new()),
    };
    Level::Needs(Fetch::ModelRoute {
        provider,
        partial: if model_partial.is_empty() {
            word.to_owned()
        } else {
            model_partial
        },
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
    let tokens = match shlex::split(line) {
        Some(tokens) => tokens,
        None => return Outcome::Notices(vec![format!("unbalanced quotes in {line:?}")]),
    };
    match tokens.first().map(String::as_str) {
        Some("/help") => Outcome::Notices(vec![HELP.to_owned()]),
        Some("/memory") => memory_command(&tokens[1..], memory_enabled),
        Some("/compact") => Outcome::Compact(compact_instructions(&tokens[1..])),
        Some("/sessions") => Outcome::Sessions(sessions_query(&tokens[1..])),
        Some("/export") => export_request(&tokens[1..]),
        Some("/fork") => Outcome::Fork,
        Some("/thinking") => match tokens[1..].first() {
            Some(level) => Outcome::Thinking(level.clone()),
            None => Outcome::Notices(vec!["usage: /thinking <level>".to_owned()]),
        },
        Some("/model") => match model_switch(&tokens[1..]) {
            Ok(switch) => Outcome::Model(switch),
            Err(usage) => Outcome::Notices(vec![usage]),
        },
        Some("/login") => Outcome::Login(login_provider(&tokens[1..])),
        Some("/logout") => Outcome::Logout(logout_provider(&tokens[1..])),
        Some("/auto") | Some("/yolo") => auto_toggle(&tokens[1..]),
        Some("/skills") if tokens.len() == 1 => Outcome::Skills,
        Some("/skills") => Outcome::Notices(vec!["usage: /skills (no arguments)".to_owned()]),
        Some(other) if other.starts_with("/skill:") => skill_command(other, line),
        Some(other) => Outcome::Notices(vec![format!("unknown command {other}; /help lists them")]),
        None => Outcome::Message,
    }
}

/// `/login [provider]`: `custom` or a built-in id skips the choice step; anything else is a
/// usage notice. More than one name is the same error.
fn login_provider(args: &[String]) -> Option<String> {
    match args {
        [] => None,
        [one] => {
            let valid = one == crate::login::CUSTOM || radian_host::descriptor(one).is_some();
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
                },
                Candidate {
                    insert: "/compact".into(),
                    description: String::new(),
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

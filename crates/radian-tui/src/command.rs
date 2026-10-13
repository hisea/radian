//! The minimal slash-command framework (ADR 0021 D7; ADR 0009 keeps the TUI full-screen).
//!
//! A submitted line whose first non-space character is `/` is a command: it never reaches the
//! model, the transcript or the session journal. `//…` is the escape — one `/` is dropped and
//! the rest is sent as an ordinary message.
//!
//! [`dispatch`] parses the line and, for `/memory`, runs the **same** `radian-host::memory`
//! command set `agent memory` uses (one clap grammar, one executor). Adding a command is one
//! arm here.

use radian_host::memory::MemoryService;
use radian_host::memory::commands::{self, CommandOutput, MemoryCommand};

/// The `/help` text. Kept short; the full grammar is `radian_host::memory::commands`.
pub const HELP: &str = "\
/help                 list the commands
/compact [note]       summarize the history now and keep the recent part (ADR 0018)
/sessions [query]     list this workspace's recent sessions, or search their text
/export [md|json] [path] [--force]
                      export this session (default md, default <workspace>/radian-<id>.<ext>)
/memory <command>     durable memory: list, search, show, add, edit, archive, restore,
                      forget, purge, confirm, export, import, project, relink, split, merge
//…                   send a message that starts with `/`";

/// A memory command waiting for the user's confirmation. Only the destructive commands
/// (`forget`, `purge`, `split`, `merge`) ask (ADR 0021 D7); the rest run immediately.
#[derive(Debug)]
pub struct PendingCommand {
    pub command: MemoryCommand,
    pub prompt: String,
}

/// What one submitted `/…` line meant.
#[derive(Debug)]
pub enum Outcome {
    /// Show these lines as notices and keep the line out of the conversation.
    Notices(Vec<String>),
    /// The command writes and must be confirmed before it runs.
    Confirm(PendingCommand),
    /// A manual `/compact` (ADR 0018): the host runs it on the runtime, between turns, and
    /// shows the outcome as a notice. `Some(instructions)` is the optional note.
    Compact(Option<String>),
    /// `/sessions [query]`: the host lists or searches this workspace's sessions. `Some` is a
    /// full-text query, `None` the recent list.
    Sessions(Option<String>),
    /// `/export …`: the host exports the current session and writes the file.
    Export(ExportRequest),
    /// Not a command after all (an empty token list); the caller sends the line.
    Message,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    List(Option<String>),
    Export(ExportRequest),
}

/// Interprets a trimmed line that starts with `/`. `memory` is the host's service, or `None`
/// when this process has no memory root.
pub fn dispatch(line: &str, memory: Option<&MemoryService>) -> Outcome {
    let tokens = match shlex::split(line) {
        Some(tokens) => tokens,
        None => return Outcome::Notices(vec![format!("unbalanced quotes in {line:?}")]),
    };
    match tokens.first().map(String::as_str) {
        Some("/help") => Outcome::Notices(vec![HELP.to_owned()]),
        Some("/memory") => memory_command(&tokens[1..], memory),
        Some("/compact") => Outcome::Compact(compact_instructions(&tokens[1..])),
        Some("/sessions") => Outcome::Sessions(sessions_query(&tokens[1..])),
        Some("/export") => export_request(&tokens[1..]),
        Some(other) => Outcome::Notices(vec![format!("unknown command {other}; /help lists them")]),
        None => Outcome::Message,
    }
}

/// The optional `/compact` note: the arguments joined with spaces, or `None` when empty.
fn compact_instructions(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let joined = joined.trim();
    (!joined.is_empty()).then(|| joined.to_owned())
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

fn memory_command(args: &[String], memory: Option<&MemoryService>) -> Outcome {
    let Some(service) = memory else {
        return Outcome::Notices(vec![
            "memory is disabled (no RADIAN_HOME or HOME)".to_owned(),
        ]);
    };
    let command = match commands::parse_args(args) {
        Ok(command) => command,
        // clap's own error/help text is shown as a notice; the program does not exit.
        Err(error) => return Outcome::Notices(vec![error.to_string().trim_end().to_owned()]),
    };
    let json = command.json();
    match commands::execute(service, command.clone(), false, json) {
        Ok(output) => match output.needs_confirmation {
            Some(prompt) => Outcome::Confirm(PendingCommand { command, prompt }),
            None => Outcome::Notices(output_notices(&output)),
        },
        Err(error) => Outcome::Notices(vec![error.message]),
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
    use radian_host::memory::{MemoryService, ScopeSelection, WriteScope};
    use radian_memory::Kind;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-tui-cmd-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(label: &str) -> (PathBuf, MemoryService) {
        let dir = scratch(label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let service = MemoryService::open(&ws, dir.join("memory"));
        service.create_project().unwrap();
        (dir, service)
    }

    #[test]
    fn help_and_unknown_commands_are_notices() {
        match dispatch("/help", None) {
            Outcome::Notices(notices) => assert!(notices[0].contains("/memory"), "{notices:?}"),
            other => panic!("expected notices, got {other:?}"),
        }
        match dispatch("/nope", None) {
            Outcome::Notices(notices) => {
                assert!(notices[0].contains("unknown command /nope"), "{notices:?}")
            }
            other => panic!("expected notices, got {other:?}"),
        }
        match dispatch("/memory add \"unbalanced", None) {
            Outcome::Notices(notices) => assert!(notices[0].contains("unbalanced quotes")),
            other => panic!("expected a quote error, got {other:?}"),
        }
    }

    #[test]
    fn memory_writes_become_a_confirmation() {
        let (dir, service) = service("confirm");
        let record = service
            .add(WriteScope::Project, Kind::Fact, None, "keep".into())
            .unwrap();
        let line = format!("/memory forget {}", record.id);
        match dispatch(&line, Some(&service)) {
            Outcome::Confirm(pending) => assert!(pending.prompt.contains("forget")),
            other => panic!("expected a confirmation, got {other:?}"),
        }
        // Nothing was written by the dispatch itself.
        assert_eq!(
            service
                .list(ScopeSelection::ProjectOnly, false)
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_and_export_parse() {
        assert!(matches!(
            dispatch("/sessions", None),
            Outcome::Sessions(None)
        ));
        match dispatch("/sessions parser bug", None) {
            Outcome::Sessions(Some(query)) => assert_eq!(query, "parser bug"),
            other => panic!("expected a sessions query, got {other:?}"),
        }
        match dispatch("/export", None) {
            Outcome::Export(request) => {
                assert_eq!(request.format, radian_host::Format::Markdown);
                assert!(request.path.is_none());
                assert!(!request.force);
            }
            other => panic!("expected an export request, got {other:?}"),
        }
        match dispatch("/export json out.json --force", None) {
            Outcome::Export(request) => {
                assert_eq!(request.format, radian_host::Format::Json);
                assert_eq!(request.path.as_deref(), Some("out.json"));
                assert!(request.force);
            }
            other => panic!("expected an export request, got {other:?}"),
        }
        // A bare path is the path, not the format.
        match dispatch("/export result.txt", None) {
            Outcome::Export(request) => {
                assert_eq!(request.format, radian_host::Format::Markdown);
                assert_eq!(request.path.as_deref(), Some("result.txt"));
            }
            other => panic!("expected an export request, got {other:?}"),
        }
        match dispatch("/export a b", None) {
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
        assert!(matches!(dispatch("/compact", None), Outcome::Compact(None)));
        match dispatch("/compact focus on the parser", None) {
            Outcome::Compact(Some(note)) => assert_eq!(note, "focus on the parser"),
            other => panic!("expected a compact request, got {other:?}"),
        }
    }

    #[test]
    fn memory_is_disabled_without_a_service() {
        match dispatch("/memory list", None) {
            Outcome::Notices(notices) => assert!(notices[0].contains("disabled")),
            other => panic!("expected a notice, got {other:?}"),
        }
    }
}

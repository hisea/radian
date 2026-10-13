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
/memory <command>     durable memory: list, search, show, add, edit, archive, restore,
                      forget, purge, confirm, export, import, project, relink, split, merge
//…                   send a message that starts with `/`";

/// A memory write waiting for the user's `y`.
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
    /// Not a command after all (an empty token list); the caller sends the line.
    Message,
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

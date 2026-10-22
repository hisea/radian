//! The worker's `radian exec` command line, checked before an attempt is claimed (ADR 0048
//! decision 7).
//!
//! `[worker] exec_args` used to reach a worker only after the attempt started: a wrong entry
//! (`-m` for `--model`) made `radian exec` exit `2` at argument parsing, the attempt ended
//! `terminated`, and every remaining attempt failed the same way. `radian arc start`,
//! `radian arc run` and `radian arc confirm` now parse the line the worker would get — the
//! wrapper's fixed arguments with placeholder values for the task and the worktree, then
//! `exec_args`, then the prompt — with the parser the worker itself runs
//! ([`radian_exec::Cli`]), before they claim or change anything. Parsing starts no session,
//! calls no model and writes no file.
//!
//! A rejected line is refused (exit `3`) with the parser's own message and names
//! `worker.exec_args`. So is a line that repeats an argument the wrapper owns
//! ([`OWNED_EXEC_ARGS`]): the wrapper's value would otherwise be overridden, or the flag given
//! twice.

use clap::Parser;

use crate::store::Error;

/// The `radian exec` arguments the wrapper fixes for every worker, with placeholder values
/// for the task (which is also the session id) and the worktree. [`check`] parses these, then
/// `[worker] exec_args`, then `-p -`, in the order [`crate::work`]'s `spawn_child` builds
/// them.
pub const FIXED_EXEC_ARGS: &[&str] = &[
    "--session-id",
    "<task>",
    "--workspace",
    "<worktree>",
    "--profile",
    "workspace",
    "--yes",
    "--output",
    "jsonl",
];

/// The prompt the worker's brief arrives on, after the caller's extra arguments.
const PROMPT_ARGS: &[&str] = &["-p", "-"];

/// The `radian exec` arguments the wrapper owns: `exec_args` may not repeat one, because the
/// wrapper's value would be overridden (or the flag given twice). One closed list: the code
/// refuses from it and the manual's drift test reads it.
pub const OWNED_EXEC_ARGS: &[&str] = &[
    "--profile",
    "--workspace",
    "--session-id",
    "--resume",
    "--output",
    "--yes",
    "-p",
];

/// The full line [`check`] parses: the program name, the wrapper's fixed arguments, the
/// caller's extra arguments and the prompt.
fn line(exec_args: &[String]) -> Vec<String> {
    std::iter::once("radian exec".to_owned())
        .chain(FIXED_EXEC_ARGS.iter().map(|arg| (*arg).to_owned()))
        .chain(exec_args.iter().cloned())
        .chain(PROMPT_ARGS.iter().map(|arg| (*arg).to_owned()))
        .collect()
}

/// Checks the worker's command line before an attempt is claimed, a `run` pass starts a task,
/// or a plan is confirmed.
///
/// # Errors
///
/// [`Error::Refused`] (exit `3`) when `exec_args` repeats an argument the wrapper owns, or
/// when the `radian exec` parser rejects the line. The message names `worker.exec_args` and,
/// for a parse failure, carries the parser's own text. Nothing is claimed or written.
pub fn check(exec_args: &[String]) -> Result<(), Error> {
    if let Some(owned) = exec_args.iter().find_map(|arg| owned_name(arg)) {
        return Err(Error::Refused(format!(
            "worker.exec_args must not repeat `{owned}`: the wrapper sets it on the worker's \
             `radian exec` line, so a value here would override the wrapper's"
        )));
    }
    radian_exec::Cli::try_parse_from(line(exec_args)).map_err(|error| {
        Error::Refused(format!(
            "worker.exec_args is not a `radian exec` argument list: {}",
            error.to_string().trim_end()
        ))
    })?;
    Ok(())
}

/// The owned argument `arg` is or sets, if any. A long flag matches its `=value` spelling
/// too; the single short flag the wrapper owns (`-p`) matches any cluster that starts with it,
/// because clap reads `-pfoo` as `-p foo`.
fn owned_name(arg: &str) -> Option<&'static str> {
    OWNED_EXEC_ARGS.iter().copied().find(|owned| {
        arg == *owned
            || (owned.starts_with("--") && arg.starts_with(&format!("{owned}=")))
            || (!owned.starts_with("--") && arg.starts_with(owned))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Exit;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// A misspelled option is the parser's own error, and the message says where it came
    /// from. Mutation: skip the parse in `check` → this fails.
    #[test]
    fn a_line_the_parser_rejects_is_refused_with_its_message() {
        let error = check(&args(&["-m", "x"])).expect_err("`-m` is not an option");
        assert_eq!(error.exit(), Exit::State, "exit 3");
        let message = error.message();
        assert!(message.contains("worker.exec_args"), "{message}");
        assert!(message.contains("'-m'"), "{message}");
    }

    /// An argument the wrapper sets is refused by name before the parser sees it (the line
    /// would parse). Mutation: drop one entry from `OWNED_EXEC_ARGS` → that case passes.
    #[test]
    fn an_argument_the_wrapper_owns_is_refused_by_name() {
        for owned in OWNED_EXEC_ARGS {
            let error = check(&[(*owned).to_owned(), "value".to_owned()])
                .expect_err("the wrapper owns this argument");
            assert_eq!(error.exit(), Exit::State, "exit 3");
            assert!(
                error.message().contains(&format!("`{owned}`")),
                "the message names `{owned}`: {}",
                error.message()
            );
        }
        assert!(
            check(&args(&["--profile=full"])).is_err(),
            "the `=value` spelling is the same argument"
        );
        let profile = check(&args(&["--profile", "full"])).expect_err("the wrapper sets --profile");
        assert!(
            profile.message().contains("must not repeat `--profile`"),
            "the owned refusal says why: {}",
            profile.message()
        );
        assert!(
            check(&args(&["-px"])).is_err(),
            "a short cluster that starts with -p sets the prompt"
        );
    }

    /// A valid extra argument list passes: the model-route tests use exactly this shape.
    #[test]
    fn a_valid_argument_list_passes() {
        check(&[]).expect("the default empty list is valid");
        check(&args(&["--model", "m", "--base-url", "http://127.0.0.1:9"]))
            .expect("a model route is valid");
    }
}

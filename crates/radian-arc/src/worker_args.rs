//! The worker's command line, checked before an attempt is claimed (ADR 0048 decision 7).
//!
//! `[worker] exec_args` used to reach a worker only after the attempt started: a wrong entry
//! (`-m` for `--model`) made `radian exec` exit `2` at argument parsing, the attempt ended
//! `terminated`, and every remaining attempt failed the same way. `radian arc start`,
//! `radian arc run` and `radian arc confirm` now check the worker line before they claim or
//! change anything. Without `[worker] command`, they parse the line the worker would get — the
//! wrapper's fixed arguments with placeholder values for the task and the worktree, then
//! `exec_args`, then the prompt — with the parser the worker itself runs
//! ([`radian_exec::Cli`]). Parsing starts no session, calls no model and writes no file.
//!
//! A rejected line is refused (exit `3`) with the parser's own message and names
//! `worker.exec_args`. So is a line that repeats an argument the wrapper owns
//! ([`OWNED_EXEC_ARGS`]): the wrapper's value would otherwise be overridden, or the flag given
//! twice.
//!
//! With `[worker] command` set, the `exec_args` parse is skipped and the program is checked
//! instead: a program with a `/` is a path resolved from the repository root that must be an
//! executable file, and any other name must be found on `PATH` as an executable file. A
//! program that does not resolve is refused (exit `3`) with a message that names
//! `worker.command` and the program.

use std::path::{Path, PathBuf};

use clap::Parser;

use crate::store::Error;
use crate::store::config::Config;

/// The `radian exec` arguments the wrapper fixes for every worker, with placeholder values
/// for the session and the worktree. [`check`] parses these, then `[worker] exec_args`, then
/// `-p -`, in the order [`crate::work`]'s `spawn_child` builds them.
pub const FIXED_EXEC_ARGS: &[&str] = &[
    "--session-id",
    "<session>",
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

/// Checks the worker line of `config` before an attempt is claimed, a `run` pass starts a task,
/// or a plan is confirmed (ADR 0048 decisions 7 and 15).
///
/// The `[worker]` keys are checked as they always were, and then every `[flows.<name>]`
/// worker is checked the same way: a flow's `command` program must resolve, and its
/// `exec_args` must be a `radian exec` argument list without an argument the wrapper owns.
///
/// # Errors
///
/// [`Error::Refused`] (exit `3`) when a `[worker]` or flow `command` program does not
/// resolve, or when an `exec_args` repeats an argument the wrapper owns or the `radian exec`
/// parser rejects the line. The message names `worker.command`, `worker.exec_args`,
/// `flows.<name>.command` or `flows.<name>.exec_args`. Nothing is claimed or written.
pub fn check_config(config: &Config, repo: &Path) -> Result<(), Error> {
    match &config.worker.command {
        Some(command) => check_command("worker.command", command, repo)?,
        None => check_named("worker.exec_args", &config.worker.exec_args)?,
    }
    for (name, flow) in &config.flows {
        check_flow(name, flow, repo)?;
    }
    Ok(())
}

/// Checks one flow's worker: its `command` program resolves, or its `exec_args` is a valid
/// `radian exec` argument list (ADR 0048 decision 15). The error names the flow's keys.
pub fn check_flow(name: &str, flow: &crate::store::config::Flow, repo: &Path) -> Result<(), Error> {
    match &flow.command {
        Some(command) => check_command(&format!("flows.{name}.command"), command, repo),
        None => check_named(&format!("flows.{name}.exec_args"), &flow.exec_args),
    }
}

/// The program and arguments the wrapper runs, with a program that contains a `/` made
/// absolute against the repository root: the wrapper's working directory is the worktree, so a
/// relative path would otherwise name a different file there than the claim checked.
pub fn resolved(command: &[String], repo: &Path) -> Vec<String> {
    let mut command = command.to_vec();
    if let Some(program) = command.first_mut()
        && program.contains('/')
        && !Path::new(program).is_absolute()
    {
        *program = repo.join(program.as_str()).display().to_string();
    }
    command
}

/// Checks that the command's program resolves (ADR 0048 decision 7). `key` names the config
/// key the error is about (`worker.command` or `flows.<name>.command`).
fn check_command(key: &str, command: &[String], repo: &Path) -> Result<(), Error> {
    let Some(program) = command.first() else {
        return Err(Error::Refused(format!(
            "{key} is empty; its first element must be the program"
        )));
    };
    if program.contains('/') {
        let path = Path::new(program);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo.join(path)
        };
        if !is_executable_file(&path) {
            return Err(Error::Refused(format!(
                "{key} names `{program}`, which is not an executable file"
            )));
        }
        return Ok(());
    }
    if find_on_path(program).is_none() {
        return Err(Error::Refused(format!(
            "{key} names `{program}`, which is not on PATH as an executable file"
        )));
    }
    Ok(())
}

/// Whether `path` is a regular file with an execute bit.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The first `PATH` entry that holds `name` as an executable file. An empty entry means the
/// current directory, as `execvp` reads it.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
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
    check_named("worker.exec_args", exec_args)
}

/// [`check`] with the config key the error names: `worker.exec_args` for `[worker]`, or
/// `flows.<name>.exec_args` for a flow (ADR 0048 decision 15).
fn check_named(key: &str, exec_args: &[String]) -> Result<(), Error> {
    if let Some(owned) = exec_args.iter().find_map(|arg| owned_name(arg)) {
        return Err(Error::Refused(format!(
            "{key} must not repeat `{owned}`: the wrapper sets it on the worker's \
             `radian exec` line, so a value here would override the wrapper's"
        )));
    }
    radian_exec::Cli::try_parse_from(line(exec_args)).map_err(|error| {
        Error::Refused(format!(
            "{key} is not a `radian exec` argument list: {}",
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

    #[cfg(unix)]
    fn executable(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the program");
        let mut permissions = std::fs::metadata(&path).expect("the program").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make it executable");
        path
    }

    /// A configured program is checked where it will run: a path is an executable file, a bare
    /// name is on `PATH` (ADR 0048 decision 7). Mutation: skip the check and a missing program
    /// is accepted.
    #[cfg(unix)]
    #[test]
    fn a_command_program_must_resolve() {
        let dir = crate::test_support::TempDir::named("radian-arc-worker-args");
        let program = executable(&dir, "worker");
        let config = |command: Vec<String>| {
            let mut config = crate::store::config::Config::default();
            config.worker.command = Some(command);
            config
        };
        check_config(&config(vec![program.display().to_string()]), &dir)
            .expect("an executable path passes");

        let error = check_config(&config(vec!["/nonexistent/worker".to_owned()]), &dir)
            .expect_err("a missing path is refused");
        assert_eq!(error.exit(), Exit::State, "exit 3");
        assert!(
            error.message().contains("worker.command"),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("/nonexistent/worker"),
            "{}",
            error.message()
        );

        // A regular file that is not executable is not a program.
        let plain = dir.join("plain");
        std::fs::write(&plain, "x").expect("the file");
        assert!(check_config(&config(vec![plain.display().to_string()]), &dir).is_err());

        // A bare name is found on the child's `PATH`; `sh` is always there.
        check_config(&config(vec!["sh".to_owned()]), &dir).expect("a bare name on PATH passes");
        let error = check_config(
            &config(vec!["definitely-not-a-program-xyz".to_owned()]),
            &dir,
        )
        .expect_err("a missing name is refused");
        assert!(
            error.message().contains("definitely-not-a-program-xyz"),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("worker.command"),
            "{}",
            error.message()
        );
    }

    /// A relative program with a `/` is made absolute from the repository root, because the
    /// wrapper's working directory is the worktree (ADR 0048 decision 7).
    #[test]
    fn a_relative_program_path_is_resolved_from_the_repository_root() {
        let repo = std::path::Path::new("/a/repo");
        assert_eq!(
            resolved(&["tools/worker".to_owned(), "--x".to_owned()], repo),
            vec!["/a/repo/tools/worker".to_owned(), "--x".to_owned()]
        );
        assert_eq!(
            resolved(&["/opt/worker".to_owned()], repo),
            vec!["/opt/worker".to_owned()],
            "an absolute path stays as it is"
        );
        assert_eq!(
            resolved(&["worker".to_owned()], repo),
            vec!["worker".to_owned()],
            "a bare name is left for PATH"
        );
    }
}

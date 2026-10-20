//! ADR 0039 decision 3: the shared `-c` overrides that keep a **host** `git` call from running
//! any command the repository's own configuration supplies, and the one place a host `git`
//! command is built.
//!
//! A host `git` call reads a `.gitattributes` the model can write (working tree, committed, or
//! `$GIT_DIR/info/attributes`, the last two also when `.git` is read-only) and a git config the
//! user installed; a `filter=<driver>` attribute then makes it run `filter.<driver>.clean`/
//! `.process` (host code execution), and `core.fsmonitor`/`core.hooksPath` do the same on
//! `git add`/`commit`/`checkout`. `GIT_ATTR_SOURCE=<empty tree>` stops only the working-tree
//! `.gitattributes`; verified on git 2.53.0 that `$GIT_DIR/info/attributes` and
//! `core.attributesFile` still apply, so the fix empties the *commands* instead.
//!
//! A **sandboxed** `git` (the `shell` tool, or anything the sandbox runs) does not use the
//! `-c` spelling below: it runs as the sandbox user, where the model controls the same
//! files, and ADR 0038 deliberately lets it run the hooks and filters the user installed.
//! In a project the user has **not** trusted (ADR 0031), the `shell` tool carries the same
//! pairs a different way — [`hardening_env`] spells them as `GIT_CONFIG_*` environment
//! entries (ADR 0039 decision 5) — so repository configuration cannot run commands through
//! a shell git either. In a trusted project nothing is injected.

use std::path::Path;
use std::process::Command;

/// The `git config` keys under `filter.<name>` that name a command.
const FILTER_KEYS: [&str; 3] = ["clean", "smudge", "process"];

fn static_pairs() -> Vec<(String, String)> {
    vec![
        ("core.fsmonitor".into(), "false".into()),
        ("core.hooksPath".into(), "/dev/null".into()),
        ("core.attributesFile".into(), "/dev/null".into()),
    ]
}

/// The emptied filter commands for `names`: every `filter.<name>.{clean,smudge,process}`
/// emptied, with `filter.<name>.required=false`, so an attributes file can still *name* a
/// driver but there is no command left to run. (An emptied command with `required` set
/// makes git fail instead of skipping it.)
fn filter_pairs(names: Vec<String>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for name in names {
        for key in FILTER_KEYS {
            pairs.push((format!("filter.{name}.{key}"), String::new()));
        }
        pairs.push((format!("filter.{name}.required"), "false".into()));
    }
    pairs
}

/// Every `(key, value)` pair the hardening sets: the static three plus the emptied filter
/// commands for each driver name in the effective config. [`host_git_overrides`] (host `-c`
/// args) and [`hardening_env`] (shell `GIT_CONFIG_*` environment) both build from here:
/// one list, two spellings.
fn config_pairs(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut pairs = static_pairs();
    pairs.extend(filter_pairs(filter_driver_names(dir)?));
    Ok(pairs)
}
/// The `-c` overrides every host git call carries ([`config_pairs`] spelled as `-c` args).
/// Build every host `git` invocation from [`host_git_command`], not from here.
///
/// - `core.fsmonitor=false`, `core.hooksPath=/dev/null`: a repo-config command never runs on
///   `git add`/`commit`/`checkout` (ADR 0039);
/// - `core.attributesFile=/dev/null`: the global attributes file cannot name a driver either;
/// - every `filter.<name>.{clean,smudge,process}` emptied, with `filter.<name>.required=false`,
///   for each name in the effective config — so an attributes file can still *name* a driver but
///   there is no command left to run.
///
/// The names are read once from the config; the model cannot write it (ADR 0038 keeps
/// `.git/config` read-only in the sandbox, and the global/system files are the user's), so the
/// set cannot be raced by a background job (ADR 0037).
pub fn host_git_overrides(dir: &Path) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = Vec::new();
    for (key, value) in config_pairs(dir)? {
        args.push("-c".into());
        args.push(format!("{key}={value}"));
    }
    Ok(args)
}

/// The same hardening as `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`
/// environment entries, for a git process the host does not build itself: the `shell` tool
/// sets these on every child it spawns in a project the user has not trusted (ADR 0039
/// decision 5), so repository configuration cannot run commands through a shell git. In a
/// trusted project nothing is set, so the user's own hooks still run.
///
/// A filter query that fails contributes no filter pairs (the git that would run them failed
/// the same way); the static three are always present.
pub fn hardening_env(dir: &Path) -> Vec<(String, String)> {
    let mut pairs = static_pairs();
    if let Ok(names) = filter_driver_names(dir) {
        pairs.extend(filter_pairs(names));
    }
    let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), pairs.len().to_string())];
    for (n, (key, value)) in pairs.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{n}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{n}"), value));
    }
    env
}

/// A host `git` [`Command`] in `dir` (`-C dir`) with [`host_git_overrides`] applied. Build every
/// host `git` invocation from here; the integration test `tests/host_git.rs` fails on a raw
/// `git` command built anywhere else under `crates/*/src`.
pub fn host_git_command(dir: &Path) -> Result<Command, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    command.args(host_git_overrides(dir)?);
    Ok(command)
}

/// A host `git` [`Command`] for a call that only *reads* attributes (`git check-attr`). It never
/// runs a filter, so it must **not** hide `core.attributesFile`: that read is how a staging
/// decision learns the true driver, and `core.attributesFile` is the user's, not the model's.
/// Hooks and fsmonitor are still off.
pub fn host_git_attributes_command(dir: &Path) -> Result<Command, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    command.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]);
    Ok(command)
}

/// The distinct `<name>`s in `filter.<name>.{clean,smudge,process}` in the effective config
/// (repo, global, system) reachable from `dir`. Exit 1 (no match) is an empty list; any other
/// failure is an error, so a broken config read can never leave a filter unguarded.
fn filter_driver_names(dir: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "config",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ])
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut names: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Each line is "filter.<name>.<key> <value>"; the first whitespace-free token is the key.
        let key = line.split_whitespace().next().unwrap_or("");
        if let Some(rest) = key.strip_prefix("filter.")
            && let Some((name, kind)) = rest.rsplit_once('.')
            && matches!(kind, "clean" | "smudge" | "process")
            && !name.is_empty()
            && !names.iter().any(|known| known == name)
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_a_filter(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radian-host-git-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .expect("git")
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q"]);
        run(&["config", "filter.evil.clean", "cat"]);
        run(&["config", "filter.evil.smudge", "cat"]);
        run(&["config", "filter.lfs.process", "git-lfs filter-process"]);
        dir
    }

    /// The dangerous repo config is off, the global attributes file is hidden, and every
    /// configured filter command is emptied (ADR 0039 decision 3).
    #[test]
    fn the_overrides_disable_the_dangerous_repo_config() {
        let dir = repo_with_a_filter("overrides");
        let overrides = host_git_overrides(&dir).expect("the overrides");
        for wanted in [
            "core.fsmonitor=false",
            "core.hooksPath=/dev/null",
            "core.attributesFile=/dev/null",
            "filter.evil.clean=",
            "filter.evil.smudge=",
            "filter.evil.process=",
            "filter.evil.required=false",
            "filter.lfs.process=",
        ] {
            assert!(
                overrides.iter().any(|arg| arg == wanted),
                "missing {wanted}: {overrides:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The attribute read must see the true attributes: it keeps `core.attributesFile` (only
    /// hooks/fsmonitor are off).
    #[test]
    fn the_attribute_command_leaves_core_attributes_file_visible() {
        let dir = repo_with_a_filter("attributes");
        let command = host_git_attributes_command(&dir).expect("the command");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "core.hooksPath=/dev/null"));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "core.attributesFile=/dev/null")
        );
        assert!(!args.iter().any(|arg| arg.starts_with("filter.")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The env spelling carries the same pairs as the `-c` spelling: the count matches the
    /// entries, and reassembling the KEY/VALUE entries gives the same list.
    #[test]
    fn the_env_spelling_carries_the_same_pairs() {
        let dir = repo_with_a_filter("env");
        let env: std::collections::HashMap<String, String> =
            hardening_env(&dir).into_iter().collect();
        let count: usize = env["GIT_CONFIG_COUNT"].parse().expect("a count");
        let mut pairs = Vec::new();
        for n in 0..count {
            pairs.push((
                env[&format!("GIT_CONFIG_KEY_{n}")].clone(),
                env[&format!("GIT_CONFIG_VALUE_{n}")].clone(),
            ));
        }
        let mut expected = config_pairs(&dir).expect("the pairs");
        pairs.sort();
        expected.sort();
        assert_eq!(pairs, expected, "one list, two spellings");
        assert!(pairs.contains(&("core.fsmonitor".to_owned(), "false".to_owned())));
        assert!(pairs.contains(&("filter.evil.clean".to_owned(), String::new())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

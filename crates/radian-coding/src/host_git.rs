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

/// The `git config` keys under `diff.<name>` that name a command: an external diff driver
/// (`command`) and a text converter (`textconv`).
const DIFF_KEYS: [&str; 2] = ["command", "textconv"];

/// What neutralises an external diff without breaking the git call (verified on git
/// 2.53.0): an empty override errors out (`cannot run : No such file...`, exit 128),
/// while `true` runs nothing, exits 0, and leaves `--stat`/`--name-only` working. A plain
/// `git diff` shows no patch for the affected paths, but the model's own `--no-ext-diff`
/// still restores the internal diff.
const DISABLE_EXTERNAL_DIFF: &str = "true";

/// What neutralises a text converter without breaking the git call (verified on git
/// 2.53.0): an empty override errors out (exit 128), while `cat` passes the content
/// through, exits 0, and the text diff is preserved.
const NEUTRAL_TEXTCONV: &str = "cat";

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

/// Every `(key, value)` pair the hardening sets: the static three, the emptied filter
/// commands for each driver name in the effective config, the neutralised external diff
/// (`diff.external`, only when the effective config sets it) and the neutralised diff
/// driver commands (`diff.<name>.command` -> `true`, `diff.<name>.textconv` -> `cat`, only
/// for the keys the effective config actually sets). [`host_git_overrides`] (host `-c`
/// args) and [`hardening_env`] (shell `GIT_CONFIG_*` environment) both build from here:
/// one list, two spellings.
fn config_pairs(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut pairs = static_pairs();
    pairs.extend(filter_pairs(filter_driver_names(dir)?));
    pairs.extend(diff_pairs(dir)?);
    Ok(pairs)
}
/// The neutralised diff pairs: `diff.external` (only when the effective config sets it —
/// a static override would suppress every patch diff, even in repos without one) and, for
/// each diff driver name in the effective config, the command keys it actually sets
/// (`command` -> `true`, `textconv` -> `cat`). Keys the config does not set are left alone:
/// adding a `command` to a driver that only sets `textconv` would hijack its diff.
fn diff_pairs(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    if config_has_value(dir, "diff.external")? {
        pairs.push(("diff.external".into(), DISABLE_EXTERNAL_DIFF.into()));
    }
    for (name, key) in section_key_entries(dir, "diff", &DIFF_KEYS)? {
        let value = if key == "textconv" {
            NEUTRAL_TEXTCONV
        } else {
            DISABLE_EXTERNAL_DIFF
        };
        pairs.push((format!("diff.{name}.{key}"), value.into()));
    }
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
///   there is no command left to run;
/// - `diff.external=true`, but only when the effective config sets it (verified on git 2.53.0:
///   an empty override errors out instead of disabling);
/// - every configured `diff.<name>.command` neutralised to `true` and every configured
///   `diff.<name>.textconv` neutralised to `cat`, so an attributes file can still *name* a
///   driver but no repo-config command runs.
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
/// A config query that fails contributes no dynamic pairs (the git that would run them
/// failed the same way); the static three are always present.
pub fn hardening_env(dir: &Path) -> Vec<(String, String)> {
    let pairs = config_pairs(dir).unwrap_or_else(|_| static_pairs());
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
    let mut names: Vec<String> = Vec::new();
    for (name, _) in section_key_entries(dir, "filter", &FILTER_KEYS)? {
        if !names.iter().any(|known| known == &name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// Whether `key` is set in the effective config reachable from `dir`: exit 0 means set
/// (the value is ignored), exit 1 means unset, anything else is an error.
fn config_has_value(dir: &Path, key: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", key])
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The distinct `(<name>, <key>)` entries in `<section>.<name>.{keys...}` in the effective
/// config (repo, global, system) reachable from `dir`: the one enumeration behind both the
/// filter names and the diff driver keys, so the key list lives in exactly one place per
/// section ([`FILTER_KEYS`], [`DIFF_KEYS`]). Exit 1 (no match) is an empty list; any other
/// failure is an error, so a broken config read can never leave a driver unguarded.
fn section_key_entries(
    dir: &Path,
    section: &str,
    keys: &[&str],
) -> Result<Vec<(String, String)>, String> {
    let pattern = format!(r"^{section}\..*\.({})$", keys.join("|"));
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get-regexp", &pattern])
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Each line is "<section>.<name>.<key> <value>"; the first whitespace-free token is the key.
        let key = line.split_whitespace().next().unwrap_or("");
        if let Some(rest) = key.strip_prefix(&format!("{section}."))
            && let Some((name, kind)) = rest.rsplit_once('.')
            && keys.contains(&kind)
            && !name.is_empty()
            && !entries
                .iter()
                .any(|known| known == &(name.to_owned(), kind.to_owned()))
        {
            entries.push((name.to_owned(), kind.to_owned()));
        }
    }
    Ok(entries)
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

    /// The dangerous repo config is off, the global attributes file is hidden, every
    /// configured filter command is emptied, and the diff commands are neutralised
    /// (ADR 0039 decision 3).
    #[test]
    fn the_overrides_disable_the_dangerous_repo_config() {
        let dir = repo_with_a_filter("overrides");
        configure_diff_commands(&dir);
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
            "diff.external=true",
            "diff.tc.textconv=cat",
            "diff.dc.command=true",
        ] {
            assert!(
                overrides.iter().any(|arg| arg == wanted),
                "missing {wanted}: {overrides:?}"
            );
        }
        // A diff driver that only sets `textconv` gets no `command` override: adding one
        // would hijack its diff.
        assert!(
            !overrides.iter().any(|arg| arg == "diff.tc.command=true"),
            "unconfigured keys stay alone: {overrides:?}"
        );
        // Without any diff config there is no diff override at all: a static
        // `diff.external` would suppress every patch diff.
        let plain_dir = repo_with_a_filter("plain");
        let plain = host_git_overrides(&plain_dir).expect("the overrides");
        assert!(
            !plain.iter().any(|arg| arg.starts_with("diff.")),
            "no diff config, no diff override: {plain:?}"
        );
        let _ = std::fs::remove_dir_all(&plain_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repo with an external diff and two diff drivers configured: `textconv` becomes
    /// `cat`, `command` becomes `true`, and nothing else is added.
    fn configure_diff_commands(dir: &std::path::Path) {
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(dir)
                    .args(args)
                    .status()
                    .expect("git")
                    .success(),
                "git {args:?}"
            );
        };
        run(&["config", "diff.external", "evil-external"]);
        run(&["config", "diff.tc.textconv", "evil-textconv"]);
        run(&["config", "diff.dc.command", "evil-command"]);
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

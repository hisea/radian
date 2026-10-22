//! ADR 0039 decision 3: every host `git` invocation is built from the one shared helper
//! (`radian_coding::host_git`), so it carries the no-filter overrides. A new raw `git` command in
//! production source must fail this test — and a `git` call is exactly what was missed before
//! (`prompt.rs`, `contrib.rs` and these tools ran `git status`/`git diff` unsandboxed).

use std::path::{Path, PathBuf};

/// A command that constructs a `git` process (any path prefix: `std::process::`, `tokio::…`).
const NEEDLE: &str = "Command::new(\"git\")";

/// Production files that may contain the needle, with the exact number of times, and why. Test
/// modules (a `#[cfg(test)]` line and everything after it) are ignored, so their setup helpers
/// need no entry here.
const ALLOWED: &[(&str, usize)] = &[
    // The helper itself: the two command builders and the two config reads.
    ("crates/radian-coding/src/host_git.rs", 4),
    // The sandbox escape-matrix harness, not a host runtime path; it only creates test repos.
    ("crates/radian-sandbox/src/harness.rs", 1),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("the workspace root")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a source dir") {
        let path = entry.expect("a dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The occurrences of [`NEEDLE`] in the part of `text` before its first `#[cfg(test)]` line.
fn production_occurrences(text: &str) -> usize {
    text.split("#[cfg(test)]")
        .next()
        .unwrap_or(text)
        .matches(NEEDLE)
        .count()
}

#[test]
fn every_host_git_call_is_built_from_the_shared_helper() {
    let root = workspace_root();
    let mut files = Vec::new();
    let crates = root.join("crates");
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let src = entry.expect("a crate").path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }

    let mut problems = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source file");
        let count = production_occurrences(&text);
        if count == 0 {
            continue;
        }
        let relative = file
            .strip_prefix(&root)
            .expect("under the workspace root")
            .to_string_lossy()
            .replace('\\', "/");
        match ALLOWED.iter().find(|(path, _)| *path == relative) {
            Some((_, allowed)) if *allowed == count => {}
            Some((_, allowed)) => problems.push(format!(
                "{relative}: {count} raw git commands before #[cfg(test)], expected {allowed}"
            )),
            None => problems.push(format!(
                "{relative}: {count} raw git command(s) not routed through radian_coding::host_git"
            )),
        }
    }
    assert!(
        problems.is_empty(),
        "every host git call must be built from radian_coding::host_git:\n{}",
        problems.join("\n")
    );

    // An allowlist that names a file which no longer exists (or was renamed) is a stale guard.
    for (path, _) in ALLOWED {
        assert!(
            files.iter().any(|file| file
                .strip_prefix(&root)
                .expect("under the workspace root")
                .to_string_lossy()
                .replace('\\', "/")
                == *path),
            "allowlisted file is gone: {path}"
        );
    }
}

/// A scratch repository whose effective config sets one filter (all three commands) and one
/// diff driver (both command keys) plus `diff.external`, so `hardening_env` emits every
/// dynamic key family the manual has to list.
fn hardening_repo() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radian-git-keys-drift-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(&dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    };
    run(&["init", "-q"]);
    for key in [
        "filter.driftprobe.clean",
        "filter.driftprobe.smudge",
        "filter.driftprobe.process",
    ] {
        run(&["config", key, "cat"]);
    }
    run(&["config", "diff.external", "driftprobe-external"]);
    run(&["config", "diff.driftprobe.command", "driftprobe-command"]);
    run(&["config", "diff.driftprobe.textconv", "driftprobe-textconv"]);
    dir
}

/// The manual's spelling of a concrete key: the driver name becomes `<name>`, because the
/// names come from the machine's effective git config (repo, global, system) and no page can
/// list them. `filter.driftprobe.clean` -> `filter.<name>.clean`; `diff.external` stays.
fn manual_spelling(key: &str) -> String {
    for section in ["filter.", "diff."] {
        if let Some(rest) = key.strip_prefix(section)
            && let Some((name, kind)) = rest.rsplit_once('.')
            && !name.is_empty()
        {
            return format!("{section}<name>.{kind}");
        }
    }
    key.to_owned()
}

/// Every git safety key a `shell` child gets in an untrusted project appears in
/// `docs/user/permissions.md`, with the value the code sets.
///
/// The source is [`radian_coding::host_git::hardening_env`] — the exact function
/// `sandboxed_command` and the approved outside-sandbox path use — so a key the code adds,
/// renames or revalues fails here until the manual follows. The concrete driver name
/// (`driftprobe`) stands in for the manual's `<name>`, and an empty value must be written
/// as `empty` in that row. Mutation: drop the `diff.<name>.textconv` row from the manual,
/// or change its value to `true`; this test fails with the key or the value.
#[test]
fn the_manual_lists_every_git_key_the_shell_sets() {
    let dir = hardening_repo();
    let doc = std::fs::read_to_string(workspace_root().join("docs/user/permissions.md"))
        .expect("permissions.md exists");
    let env: std::collections::HashMap<String, String> =
        radian_coding::host_git::hardening_env(&dir)
            .into_iter()
            .collect();
    let count: usize = env["GIT_CONFIG_COUNT"].parse().expect("a count");
    let mut pairs = Vec::new();
    for n in 0..count {
        pairs.push((
            env[&format!("GIT_CONFIG_KEY_{n}")].clone(),
            env[&format!("GIT_CONFIG_VALUE_{n}")].clone(),
        ));
    }
    // The trap itself works: the dynamic families are really in the env, not a static fallback.
    for key in [
        "filter.driftprobe.clean",
        "diff.external",
        "diff.driftprobe.command",
        "diff.driftprobe.textconv",
    ] {
        assert!(
            pairs.iter().any(|(name, _)| name == key),
            "hardening_env no longer sets {key}: {pairs:?}"
        );
    }
    for name in [
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_<n>",
        "GIT_CONFIG_VALUE_<n>",
    ] {
        assert!(
            doc.contains(name),
            "permissions.md never names the `{name}` spelling the shell sets"
        );
    }
    for (key, value) in pairs {
        let manual_key = manual_spelling(&key);
        let row = doc
            .lines()
            .find(|line| {
                line.starts_with('|')
                    && line
                        .split('|')
                        .nth(1)
                        .is_some_and(|cell| cell.contains(&format!("`{manual_key}`")))
            })
            .unwrap_or_else(|| {
                panic!("permissions.md has no row for `{manual_key}`, set by hardening_env")
            });
        let expected = if value.is_empty() {
            "empty".to_owned()
        } else {
            format!("`{value}`")
        };
        assert!(
            row.contains(&expected),
            "the `{manual_key}` row does not say {expected}: `{row}`"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

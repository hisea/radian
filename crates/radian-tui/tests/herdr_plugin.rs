//! The herdr plugin (ADR 0034): the manifest herdr reads and the radian
//! subcommands its panes run.
//!
//! `herdr plugin link` cannot run in this suite: the CLI talks to the herdr server that is running
//! for whoever started the test, and linking would register into that user's own config. The
//! manifest is therefore validated field by field here, against the fields herdr 0.9.5 requires
//! (the `InstalledPluginInfo` schema and its link errors). A link into a throwaway config
//! directory was run by hand and is recorded in `MUTATIONS.md`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integrations/herdr")
}

fn manifest() -> toml::Value {
    let path = plugin_root().join("herdr-plugin.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.parse::<toml::Value>()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn array<'a>(value: &'a toml::Value, key: &str) -> &'a Vec<toml::Value> {
    value
        .get(key)
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("no {key} array"))
}

fn string(value: &toml::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("no {key} string in {value}"))
        .to_owned()
}

/// The fields herdr 0.9.5 requires of a manifest (`invalid_plugin_name`, `plugin version is
/// required`, `plugin min_herdr_version is required`) plus the ones the ADR names.
#[test]
fn the_manifest_has_the_fields_herdr_requires() {
    let manifest = manifest();
    assert_eq!(string(&manifest, "id"), "radian");
    assert_eq!(string(&manifest, "name"), "Radian");
    assert!(!string(&manifest, "version").is_empty(), "a version");
    assert_eq!(
        string(&manifest, "min_herdr_version"),
        "0.9.5",
        "the pinned minimum (decision 4)"
    );
    assert!(
        !string(&manifest, "description").is_empty(),
        "a description"
    );

    let ids = |key: &str| -> Vec<String> {
        array(&manifest, key)
            .iter()
            .map(|entry| string(entry, "id"))
            .collect()
    };
    assert_eq!(ids("actions"), vec!["start", "start-worktree"]);
    for action in array(&manifest, "actions") {
        assert!(!string(action, "title").is_empty(), "an action title");
        assert!(!array(action, "command").is_empty(), "an action command");
    }
    assert_eq!(ids("panes"), vec!["sessions", "attach"]);
    for pane in array(&manifest, "panes") {
        assert_eq!(string(pane, "placement"), "split", "decision 4's placement");
        assert!(!array(pane, "command").is_empty(), "a pane command");
    }
    let events = array(&manifest, "events");
    assert_eq!(events.len(), 1);
    assert_eq!(string(&events[0], "on"), "worktree.created");
    // Decision 4: no default key bindings, and no link handlers.
    assert!(manifest.get("keys").is_none(), "no key bindings ship");
    assert!(manifest.get("link_handlers").is_none(), "no link handlers");
}

/// Every `scripts/...` a command names exists, is executable, and is a POSIX shell script; and the
/// scripts execute no program other than `$HERDR_BIN_PATH` and `radian` (decision 4: they never
/// read radian's session files or credentials). The program check is a guard against the obvious
/// helpers, not a shell parser.
#[test]
fn the_scripts_are_sh_and_call_only_herdr_and_radian() {
    let manifest = manifest();
    let mut scripts: Vec<String> = Vec::new();
    for key in ["actions", "events", "panes"] {
        for entry in array(&manifest, key) {
            for word in array(entry, "command") {
                let word = word.as_str().expect("a command word").to_owned();
                if word.ends_with(".sh") {
                    scripts.push(word);
                }
            }
        }
    }
    assert!(!scripts.is_empty(), "the plugin runs scripts");

    for script in scripts {
        let path = plugin_root().join(&script);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(text.starts_with("#!/bin/sh\n"), "{script} is a sh script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "{script} is executable");
        }
        for helper in [
            "jq", "python", "curl", "sed ", "awk", "grep ", "cat ", "tr ",
        ] {
            assert!(
                !text.contains(helper),
                "{script} must not call {helper:?}: {text}"
            );
        }
        // The only programs it runs are herdr's own binary and radian.
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("exec ") {
                assert!(
                    rest.starts_with("\"$HERDR_BIN_PATH\"") || rest.starts_with("radian "),
                    "{script}: exec {rest}"
                );
            }
        }
    }
}

/// The two pane commands are plain radian subcommands, so they run without herdr: `--list` prints
/// the workspace's session table, and `attach <id> --follow` refuses an unknown id instead of
/// misreading the flag.
#[test]
fn the_pane_commands_run_standalone() {
    let dir = std::env::temp_dir().join(format!("radian-plugin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let radian = env!("CARGO_BIN_EXE_radian");
    let dir = dir.to_str().unwrap();

    let list = Command::new(radian)
        .args(["--list", "--session-dir", dir])
        .output()
        .expect("run radian --list");
    assert!(
        list.status.success(),
        "radian --list: {}{}",
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&list.stderr)
    );

    // `--follow` and the positional id parse; the refusal names the id (exit 2, ADR 0032 A3).
    let attach = Command::new(radian)
        .args([
            "attach",
            "plugin-standalone",
            "--follow",
            "--session-dir",
            dir,
        ])
        .output()
        .expect("run radian attach");
    assert_eq!(attach.status.code(), Some(2), "{attach:?}");
    let stderr = String::from_utf8_lossy(&attach.stderr);
    assert!(
        stderr.contains("no session \"plugin-standalone\""),
        "the attach refusal: {stderr}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

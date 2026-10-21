//! The manual stays in sync with the code: every slash command is documented,
//! and `radian docs` reads the same table `read` does.

use radian_tui::command;

/// Every slash command appears in `docs/user/commands.md`.
#[test]
fn every_slash_command_appears_in_the_manual() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let commands_md = std::fs::read_to_string(manifest.join("../../docs/user/commands.md"))
        .expect("commands.md exists");
    for command in command::tree() {
        // `skill:` is glued with a colon; its manual form is `/skill:<name>`.
        let token = if command.name == "skill:" {
            "/skill:".to_owned()
        } else {
            format!("/{}", command.name)
        };
        assert!(
            commands_md.contains(&token),
            "/{} is missing from commands.md",
            command.name
        );
    }
    // The `/help` hint points at the manual.
    assert!(
        command::HELP.contains("Ask about radian itself"),
        "HELP hints at the manual"
    );
}

/// Every argument choice the command registry lists appears in `docs/user/commands.md`.
///
/// The registry's `Ready` candidates are the source: calling each level with the
/// previous level's choices as context yields exactly the words completion offers
/// without host I/O. Levels that need the host (theme files, models, skill names,
/// record ids) are not listed in the registry, so the test skips them: prose covers
/// those, and this test covers everything data alone can offer.
#[test]
fn every_command_argument_choice_appears_in_the_manual() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let commands_md = std::fs::read_to_string(manifest.join("../../docs/user/commands.md"))
        .expect("commands.md exists");
    for cmd in command::tree() {
        // `/login` offers the provider ids: they are host data (the compiled-in
        // descriptors plus the login entries), documented in `providers.md`,
        // not argument words the registry owns. Everything else `Ready` here is
        // a fixed word the manual must name.
        if cmd.name == "login" {
            continue;
        }
        let mut typeds: Vec<Vec<String>> = vec![Vec::new()];
        for (depth, source) in cmd.levels.iter().enumerate() {
            let mut next: Vec<Vec<String>> = Vec::new();
            let mut listed_any = false;
            for typed in &typeds {
                let borrowed: Vec<&str> = typed.iter().map(String::as_str).collect();
                if let command::Level::Ready(candidates) = source(&borrowed, "") {
                    listed_any = true;
                    for candidate in &candidates {
                        assert!(
                            commands_md.contains(&candidate.insert),
                            "/{} level {depth} choice {:?} is missing from commands.md",
                            cmd.name,
                            candidate.insert
                        );
                        let mut extended = typed.clone();
                        extended.push(candidate.insert.clone());
                        next.push(extended);
                    }
                }
            }
            // A level that needs the host (or takes free text) stops the probe: there
            // is nothing data alone lists at this depth.
            if !listed_any {
                break;
            }
            typeds = next;
        }
    }
}

/// Every rebindable key binding appears in `docs/user/keybindings.md`.
///
/// The source is the same three tables the keymap is built from
/// ([`radian_tui::keys::builtin_keys`]), so the test cannot drift from the
/// behavior: adding or removing a binding changes the keymap itself. The fixed
/// (non-rebindable) keys come from [`radian_tui::keys::FIXED_KEYS`], a descriptive
/// table the manual pins: changing behavior in the dialogs, picker, inspector,
/// `btw` overlay or login wizard means updating it there too.
#[test]
fn every_key_binding_appears_in_the_manual() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let keys_md = std::fs::read_to_string(manifest.join("../../docs/user/keybindings.md"))
        .expect("keybindings.md exists");
    for (action, keys) in radian_tui::keys::builtin_keys() {
        assert!(
            keys_md.contains(&format!("`{action}`")),
            "action `{action}` is missing from keybindings.md"
        );
        for key in keys {
            assert!(
                keys_md.contains(&format!("`{key}`")),
                "key `{key}` of action `{action}` is missing from keybindings.md"
            );
        }
    }
    for (area, key) in radian_tui::keys::FIXED_KEYS {
        assert!(
            keys_md.contains(&format!("`{key}`")),
            "fixed key `{key}` ({area}) is missing from keybindings.md"
        );
    }
}

/// Every hook event name appears in `docs/user/hooks.md`.
///
/// The source is the one constant both hook readers (the user `[hooks]` section
/// and the project `hooks.json`) match on, so a new event means a new constant
/// first, and the test fails until the manual names it.
#[test]
fn every_hook_event_name_appears_in_the_manual() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let hooks_md = std::fs::read_to_string(manifest.join("../../docs/user/hooks.md"))
        .expect("hooks.md exists");
    for event in radian_host::config::HOOK_EVENTS {
        assert!(
            hooks_md.contains(&format!("`{event}`")),
            "event `{event}` is missing from hooks.md"
        );
    }
}

/// Every theme role the loader knows appears in `docs/user/themes.md`.
///
/// The source is [`radian_tui::theme::Role::ALL`], the same table the loader parses
/// against, so a new role fails this test until the manual documents what it paints.
#[test]
fn every_theme_role_appears_in_the_manual() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let themes_md = std::fs::read_to_string(manifest.join("../../docs/user/themes.md"))
        .expect("themes.md exists");
    for role in radian_tui::theme::Role::ALL {
        assert!(
            themes_md.contains(&format!("`{}`", role.as_str())),
            "role `{}` is missing from themes.md",
            role.as_str()
        );
    }
}

/// `radian docs` and `read radian://docs/` share one table.
#[test]
fn docs_topics_match_embedded_pages() {
    for (name, text) in radian_coding::docs::DOCS {
        let topic = radian_coding::docs::topic(name);
        let (found_name, found_text) = radian_coding::docs::lookup_topic(topic)
            .unwrap_or_else(|| panic!("topic {topic} resolves"));
        assert_eq!(found_name, *name);
        assert_eq!(found_text, *text, "topic {topic} text matches");
        // The `.md` spelling resolves too.
        let (_, by_file) = radian_coding::docs::lookup_topic(name).expect("by file name");
        assert_eq!(by_file, *text);
        // The frontmatter description is present for the index.
        assert!(
            !radian_coding::docs::description(text).is_empty(),
            "{name} has a description"
        );
    }
    // An unknown topic resolves to nothing (the CLI exits 2 and lists topics).
    assert!(radian_coding::docs::lookup_topic("nope").is_none());
}

/// `radian docs` prints the index; a topic prints the page verbatim.
#[test]
fn radian_docs_prints_index_and_pages() {
    let bin = env!("CARGO_BIN_EXE_radian");
    // No arguments: one line per topic with its description.
    let index = std::process::Command::new(bin)
        .arg("docs")
        .output()
        .expect("radian docs runs");
    assert!(index.status.success(), "{index:?}");
    let out = String::from_utf8_lossy(&index.stdout);
    for (name, text) in radian_coding::docs::DOCS {
        let topic = radian_coding::docs::topic(name);
        assert!(out.contains(topic), "index lists {topic}");
        assert!(
            out.contains(radian_coding::docs::description(text)),
            "index describes {topic}"
        );
    }
    // A topic prints the page verbatim (the same table `read` uses).
    let page = std::process::Command::new(bin)
        .arg("docs")
        .arg("commands")
        .output()
        .expect("radian docs commands runs");
    assert!(page.status.success(), "{page:?}");
    let body = String::from_utf8_lossy(&page.stdout);
    let (_, expected) = radian_coding::docs::lookup_topic("commands").unwrap();
    assert_eq!(body, *expected, "page bytes match the table");
}

/// An unknown topic exits 2 and lists the pages.
#[test]
fn radian_docs_unknown_topic_exits_two_and_lists_pages() {
    let bin = env!("CARGO_BIN_EXE_radian");
    let missing = std::process::Command::new(bin)
        .arg("docs")
        .arg("nope")
        .output()
        .expect("radian docs nope runs");
    assert_eq!(missing.status.code(), Some(2), "{missing:?}");
    let err = String::from_utf8_lossy(&missing.stderr);
    for (name, _) in radian_coding::docs::DOCS {
        let topic = radian_coding::docs::topic(name);
        assert!(err.contains(topic), "stderr lists {topic}: {err}");
    }
}

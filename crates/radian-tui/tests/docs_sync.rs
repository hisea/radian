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

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

/// Every footer item id the component registry lists appears in `docs/user/footer.md`.
///
/// The source is [`radian_tui::footer::footer_item_ids`], the registry order itself, so a new
/// built-in fails this test until the manual gives it a section.
#[test]
fn every_footer_item_id_appears_in_the_manual() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let footer_md = std::fs::read_to_string(manifest.join("../../docs/user/footer.md"))
        .expect("footer.md exists");
    for id in radian_tui::footer::footer_item_ids() {
        assert!(
            footer_md.contains(&format!("`{id}`")),
            "item `{id}` is missing from footer.md"
        );
    }
    // The command item is no built-in: its section is pinned by name.
    assert!(
        footer_md.contains("### Command items"),
        "the command item section is missing from footer.md"
    );
}

/// Every parameter each footer item reads appears in that item's section of
/// `docs/user/footer.md`.
///
/// The source is [`radian_tui::footer::footer_item_params`]: each component declares its own
/// list by overriding `Component::params` (`context`'s `width`/`fill`/`empty`, `status`'s
/// `keys`), so the test cannot drift from the behavior. A command item's `command`/`period` come from
/// [`radian_host::command_footer::COMMAND_ITEM_PARAMS`], the same list `command_specs` reads.
#[test]
fn every_footer_item_parameter_appears_in_its_section() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let footer_md = std::fs::read_to_string(manifest.join("../../docs/user/footer.md"))
        .expect("footer.md exists");
    for id in radian_tui::footer::footer_item_ids() {
        let section = item_section(&footer_md, id);
        for param in radian_tui::footer::footer_item_params(id).unwrap_or(&[]) {
            assert!(
                section.contains(&format!("`{param}`")),
                "item `{id}` reads `{param}` but its section does not name it"
            );
        }
    }
    let commands = item_section(&footer_md, "Command items");
    for param in radian_host::command_footer::COMMAND_ITEM_PARAMS {
        assert!(
            commands.contains(&format!("`{param}`")),
            "a command item reads `{param}` but its section does not name it"
        );
    }
}

/// Every placeholder each footer item offers appears in its section of
/// `docs/user/footer.md`.
///
/// The source is [`radian_tui::footer::footer_item_placeholders`], the registry's own table, so
/// a new placeholder fails this test until the manual documents it.
#[test]
fn every_footer_item_placeholder_appears_in_its_section() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let footer_md = std::fs::read_to_string(manifest.join("../../docs/user/footer.md"))
        .expect("footer.md exists");
    for id in radian_tui::footer::footer_item_ids() {
        let section = item_section(&footer_md, id);
        for placeholder in radian_tui::footer::footer_item_placeholders(id).unwrap_or(&[]) {
            assert!(
                section.contains(&format!("`{placeholder}`")),
                "item `{id}` offers `{placeholder}` but its section does not name it"
            );
        }
    }
}

/// Each preset's fenced block in `docs/user/footer.md` equals `radian footer export <preset>`
/// byte for byte.
///
/// The source is [`radian_tui::footer::export_preset`], the same function the CLI prints, so the
/// embedded TOML cannot drift from the shipped presets.
#[test]
fn every_footer_preset_block_matches_its_export() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let footer_md = std::fs::read_to_string(manifest.join("../../docs/user/footer.md"))
        .expect("footer.md exists");
    for preset in radian_tui::footer::Preset::ALL {
        let preset = preset.as_str();
        let section = item_section(&footer_md, &format!("Preset `{preset}`"));
        let block = fenced_toml(section);
        let exported = radian_tui::footer::export_preset(preset).expect("a built-in preset");
        assert_eq!(
            block, exported,
            "the `{preset}` block in footer.md differs from `radian footer export {preset}`"
        );
    }
}

/// Every allowed value of `position`, `glyphs`, `context_line` and `preset` appears in that
/// field's own `[tui.footer]` row of `docs/user/footer.md`.
///
/// Each source is the one `ALL` list its parser looks up (`as_str` is exhaustive, so a new
/// variant fails to compile until it gains a spelling), so a new value fails this test until
/// the manual names it. Each value is asserted in its key's row, so a mention elsewhere on
/// the page does not satisfy it. Each key's table is located by its own heading; a row in
/// another table never counts.
#[test]
fn every_footer_closed_set_value_appears_in_its_row() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let footer_md = std::fs::read_to_string(manifest.join("../../docs/user/footer.md"))
        .expect("footer.md exists");
    for (heading, key, values) in [
        (
            "## Rows",
            "position",
            radian_tui::footer::RowPosition::ALL
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "## `[tui.footer]`",
            "glyphs",
            radian_tui::footer::Glyphs::ALL
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "## `[tui.footer]`",
            "context_line",
            radian_tui::footer::ContextLine::ALL
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "## `[tui.footer]`",
            "preset",
            radian_tui::footer::Preset::ALL
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        let rows = footer_rows_under(&footer_md, heading);
        let row = footer_row(&rows, key);
        for value in values {
            assert!(
                row.iter().any(|cell| cell.contains(&format!("`{value}`"))),
                "`{value}` is missing from the `{key}` row: {row:?}"
            );
        }
    }
}

/// Every `[tui.footer]` default in `docs/user/footer.md` matches the code.
///
/// The source is an empty config through [`radian_tui::footer::Footer::from_config`]: the preset,
/// glyphs, gap fill, transparency and separator the code falls back to. Each default is asserted
/// in its key's own table row, so a value listed elsewhere on the page does not satisfy it. The
/// table is located by its `## `[tui.footer]`` heading; a row in another table never counts.
#[test]
fn every_footer_default_matches_the_code() {
    use radian_host::config::TuiFooterConfig;
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let footer_md = std::fs::read_to_string(manifest.join("../../docs/user/footer.md"))
        .expect("footer.md exists");
    let rows = footer_field_rows(&footer_md);
    let (footer, _) = radian_tui::footer::Footer::from_config(&TuiFooterConfig::default());
    // The fixture parses an empty config, so the built footer carries the code defaults.
    assert!(
        !footer.transparent(),
        "the empty config leaves transparency off"
    );
    for (key, expected) in [
        (
            "preset",
            format!("`{}`", radian_tui::footer::Preset::default().as_str()),
        ),
        (
            "glyphs",
            format!("`{}`", radian_tui::footer::Glyphs::default().as_str()),
        ),
        (
            "context_line",
            format!("`{}`", radian_tui::footer::ContextLine::default().as_str()),
        ),
        (
            "transparent",
            if footer.transparent() {
                "`true`".to_string()
            } else {
                "`false`".to_string()
            },
        ),
        // The page shows the separator quoted inside backticks: `" · "`.
        (
            "separator",
            format!("`\"{}\"`", radian_tui::footer::DEFAULT_SEPARATOR),
        ),
        (
            "separator_style",
            format!("`{}`", radian_tui::footer::DEFAULT_SEPARATOR_STYLE.as_str()),
        ),
    ] {
        let row = footer_row(&rows, key);
        let default_cell = row
            .get(2)
            .unwrap_or_else(|| panic!("`{key}` row has no Default column: {row:?}"));
        assert_eq!(
            default_cell, &expected,
            "`{key}` row default is `{default_cell}` but the code default is `{expected}`"
        );
    }
}

/// The `[tui.footer]` field table's data rows, each split into trimmed cells. The heading is
/// the anchor: a row in another table on the page never satisfies a check. A missing heading
/// or table fails with the page and the heading.
fn footer_field_rows(footer_md: &str) -> Vec<Vec<String>> {
    footer_rows_under(footer_md, "## `[tui.footer]`")
}

/// The table's data rows under one `footer.md` heading. The heading is the anchor: a row in
/// another table never satisfies a check. A missing heading or table fails with the page and
/// the heading.
fn footer_rows_under(footer_md: &str, heading: &str) -> Vec<Vec<String>> {
    radian_coding::drift::tables_under(footer_md, "footer.md", heading)
        .into_iter()
        .flatten()
        .collect()
}

/// The `[tui.footer]` row whose first cell is exactly `` `key` ``.
fn footer_row<'a>(rows: &'a [Vec<String>], key: &str) -> &'a Vec<String> {
    let wanted = format!("`{key}`");
    rows.iter()
        .find(|row| row.first().is_some_and(|cell| *cell == wanted))
        .unwrap_or_else(|| panic!("footer.md's `[tui.footer]` table has no `{key}` row"))
}

/// The `### <name>` section of `footer.md`: from its heading to the next `### ` or `## ` heading.
/// Item sections are backticked (`### `brand``); other sections are not.
fn item_section<'a>(footer_md: &'a str, name: &str) -> &'a str {
    let heading = format!("### `{name}`");
    let (heading, start) = match footer_md.find(&heading) {
        Some(index) => (heading, index),
        None => {
            let plain = format!("### {name}");
            let index = footer_md
                .find(&plain)
                .unwrap_or_else(|| panic!("footer.md has no section `{plain}`"));
            (plain, index)
        }
    };
    let rest = &footer_md[start + heading.len()..];
    let end = rest
        .find("\n### ")
        .or_else(|| rest.find("\n## "))
        .map(|index| start + heading.len() + index)
        .unwrap_or(footer_md.len());
    &footer_md[start..end]
}

/// The first fenced `toml` block of a section, with its trailing newline restored.
fn fenced_toml(section: &str) -> String {
    let start = section
        .find("```toml")
        .expect("the section has a toml block");
    let after = &section[start + "```toml".len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("\n```").expect("the toml block is closed");
    format!("{}\n", &after[..end])
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

//! The bundled user manual (`docs/user/*.md`), compiled into the binary.
//!
//! The pages live in `docs/user/` as English Markdown. This table embeds each page with
//! `include_str!`, so the binary and the source tree always carry the same text. No file
//! is written at startup and nothing is read from `~/.radian/`: the pages come from
//! memory only.
//!
//! The virtual root is `radian://docs/`. `read` opens `radian://docs/<page>.md` and lists
//! `radian://docs/`. Every other file tool refuses the prefix.

/// The virtual root, with the trailing slash. `radian://docs` without it names the same root.
pub const DOCS_PREFIX: &str = "radian://docs/";
/// The bare prefix every virtual path starts with (`write`/`patch` refuse it).
pub const VIRTUAL_PREFIX: &str = "radian://";

/// One page: the file name and its full text, frontmatter included.
pub const DOCS: &[(&str, &str)] = &[
    ("cli.md", include_str!("../../../docs/user/cli.md")),
    (
        "commands.md",
        include_str!("../../../docs/user/commands.md"),
    ),
    (
        "configuration.md",
        include_str!("../../../docs/user/configuration.md"),
    ),
    (
        "context-files.md",
        include_str!("../../../docs/user/context-files.md"),
    ),
    (
        "environment.md",
        include_str!("../../../docs/user/environment.md"),
    ),
    (
        "extensions.md",
        include_str!("../../../docs/user/extensions.md"),
    ),
    ("footer.md", include_str!("../../../docs/user/footer.md")),
    ("hooks.md", include_str!("../../../docs/user/hooks.md")),
    ("index.md", include_str!("../../../docs/user/index.md")),
    (
        "keybindings.md",
        include_str!("../../../docs/user/keybindings.md"),
    ),
    ("mcp.md", include_str!("../../../docs/user/mcp.md")),
    ("memory.md", include_str!("../../../docs/user/memory.md")),
    (
        "permissions.md",
        include_str!("../../../docs/user/permissions.md"),
    ),
    (
        "pi-migration.md",
        include_str!("../../../docs/user/pi-migration.md"),
    ),
    (
        "providers.md",
        include_str!("../../../docs/user/providers.md"),
    ),
    (
        "quickstart.md",
        include_str!("../../../docs/user/quickstart.md"),
    ),
    (
        "sessions.md",
        include_str!("../../../docs/user/sessions.md"),
    ),
    ("skills.md", include_str!("../../../docs/user/skills.md")),
    (
        "subagent-and-jobs.md",
        include_str!("../../../docs/user/subagent-and-jobs.md"),
    ),
    ("themes.md", include_str!("../../../docs/user/themes.md")),
    (
        "troubleshooting.md",
        include_str!("../../../docs/user/troubleshooting.md"),
    ),
];

/// The page text, or `None` when no page has this name.
pub fn find(name: &str) -> Option<&'static str> {
    DOCS.iter().find(|(n, _)| *n == name).map(|(_, text)| *text)
}

/// Every page name, in table order (already sorted).
pub fn names() -> Vec<&'static str> {
    DOCS.iter().map(|(name, _)| *name).collect()
}

/// Whether `user` is the docs root (`radian://docs` or `radian://docs/`).
pub fn is_root(user: &str) -> bool {
    user == "radian://docs" || user == "radian://docs/"
}

/// The page name for `radian://docs/<page>`, or `None` when `user` is not under the root.
pub fn page_name(user: &str) -> Option<&str> {
    user.strip_prefix(DOCS_PREFIX)
}

/// Whether `user` starts with the virtual prefix (`radian://`).
pub fn is_virtual(user: &str) -> bool {
    user.starts_with(VIRTUAL_PREFIX)
}

/// The topic for a page file name (`commands.md` -> `commands`).
pub fn topic(name: &str) -> &str {
    name.strip_suffix(".md").unwrap_or(name)
}

/// The frontmatter `description:` of a page, or an empty string when missing.
pub fn description(text: &str) -> &str {
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return "";
    }
    for line in lines {
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("description:") {
            return rest.trim();
        }
    }
    ""
}

/// Looks up a `radian docs` topic: `commands` or `commands.md` both find the page.
pub fn lookup_topic(topic: &str) -> Option<(&'static str, &'static str)> {
    let name = if topic.ends_with(".md") {
        topic.to_owned()
    } else {
        format!("{topic}.md")
    };
    find(&name).map(|text| {
        let page = DOCS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(n, _)| *n)
            .unwrap_or("");
        (page, text)
    })
}

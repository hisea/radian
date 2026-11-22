//! The bundled user manuals (`docs/user/`), compiled into the binary.
//!
//! The pages live in `docs/user/` as English Markdown. This table embeds each page with
//! `include_str!`, so the binary and the source tree always carry the same text. No file
//! is written at startup and nothing is read from `~/.radian/`: the pages come from
//! memory only.
//!
//! The pages make up the radian manual, with its own size budget. [`Manual`] is the one place
//! that decides membership.
//!
//! A page name is its path under `docs/user/` (`commands.md`). The virtual
//! root is `radian://docs/`. `read` opens `radian://docs/<page>.md`, lists `radian://docs/`,
//! and opens a directory's `index.md` for `radian://docs/<dir>` or `radian://docs/<dir>/`.
//! Every other file tool refuses the prefix.

/// The virtual root, with the trailing slash. `radian://docs` without it names the same root.
pub const DOCS_PREFIX: &str = "radian://docs/";
/// The bare prefix every virtual path starts with (`write`/`patch` refuse it).
pub const VIRTUAL_PREFIX: &str = "radian://";

/// One page: the file name (its path under `docs/user/`) and its full text, frontmatter
/// included.
pub const DOCS: &[(&str, &str)] = &[
    ("acp.md", include_str!("../../../docs/user/acp.md")),
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
    ("control.md", include_str!("../../../docs/user/control.md")),
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
    ("tools.md", include_str!("../../../docs/user/tools.md")),
    (
        "troubleshooting.md",
        include_str!("../../../docs/user/troubleshooting.md"),
    ),
];

/// The manuals the binary carries: the radian manual.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manual {
    /// Every page, mapped by `index.md`.
    Radian,
}

impl Manual {
    /// Every manual.
    pub const ALL: [Manual; 1] = [Manual::Radian];

    /// The name a failure or a listing uses.
    pub fn name(self) -> &'static str {
        match self {
            Manual::Radian => "radian manual",
        }
    }

    /// The page that maps this manual. A mapping check leaves this page out.
    pub fn index(self) -> &'static str {
        match self {
            Manual::Radian => "index.md",
        }
    }
}

/// The manual a page belongs to. This is the one place membership is decided.
pub fn manual_of(_name: &str) -> Manual {
    Manual::Radian
}

/// Every page of one manual, in table order.
pub fn pages_of(manual: Manual) -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    DOCS.iter()
        .filter(move |(name, _)| manual_of(name) == manual)
}

/// The embedded bytes one manual carries.
pub fn size_of(manual: Manual) -> usize {
    pages_of(manual).map(|(_, text)| text.len()).sum()
}

/// The size cap one manual may reach (512 KiB). Each manual has its own budget.
pub const MANUAL_SIZE_CAP: usize = 512 * 1024;

/// The failure message when one manual is over its cap, or `None` when it fits. The message
/// names the manual and the size.
pub fn cap_failure(manual: Manual, total: usize) -> Option<String> {
    (total > MANUAL_SIZE_CAP).then(|| {
        format!(
            "the {} is {total} bytes, over the {MANUAL_SIZE_CAP}-byte cap",
            manual.name()
        )
    })
}

/// The index list item that names a page: a dash, the backticked topic, and a colon.
fn index_line(name: &str) -> String {
    format!("- `{}`:", topic(name))
}

/// The pages of `manual` that its own index does not list, in table order. The index page
/// itself is not listed by itself, so it never counts as missing.
pub fn pages_missing_from_index(manual: Manual, index: &str) -> Vec<&'static str> {
    let own = manual.index();
    pages_of(manual)
        .map(|(name, _)| *name)
        .filter(|name| *name != own && !index.contains(&index_line(name)))
        .collect()
}

/// The `radian docs` line for a page: `topic: description`, or just `topic` when the page
/// has no frontmatter description.
pub fn topic_line(name: &str, text: &str) -> String {
    let topic = topic(name);
    match description(text) {
        "" => topic.to_owned(),
        description => format!("{topic}: {description}"),
    }
}

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

/// The page name for a `radian://docs/<path>`, or `None` when `user` is not under the root.
/// A path that does not end in `.md` names a directory, whose page is its `index.md`
/// (`radian://docs/dir` and `radian://docs/dir/` both name `dir/index.md`).
pub fn page_name(user: &str) -> Option<String> {
    let rest = user.strip_prefix(DOCS_PREFIX)?;
    Some(directory_page(rest).unwrap_or_else(|| rest.to_owned()))
}

/// The page a bundled-manual directory names (`radian://docs/dir/` -> `dir/index.md`),
/// or `None` when `user` is not a bundled-manual directory.
pub fn dir_page(user: &str) -> Option<String> {
    let rest = user.strip_prefix(DOCS_PREFIX)?;
    directory_page(rest)
}

/// The `index.md` of a directory path, or `None` when `rest` names a file (it ends in
/// `.md`) or is the root (handled by [`is_root`]).
fn directory_page(rest: &str) -> Option<String> {
    if rest.is_empty() || rest.ends_with(".md") {
        return None;
    }
    Some(format!("{}/index.md", rest.trim_end_matches('/')))
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

/// Looks up a `radian docs` topic: `commands` or `commands.md` both find the page, and a
/// topic that names a directory opens its `index.md` (`dir` -> `dir/index.md`).
pub fn lookup_topic(topic: &str) -> Option<(&'static str, &'static str)> {
    let name = if topic.ends_with(".md") {
        topic.to_owned()
    } else {
        format!("{topic}.md")
    };
    find_entry(&name).or_else(|| {
        let dir = topic.trim_end_matches('/');
        if dir.is_empty() {
            return None;
        }
        find_entry(&format!("{dir}/index.md"))
    })
}

/// One table entry by exact page name.
fn find_entry(name: &str) -> Option<(&'static str, &'static str)> {
    DOCS.iter()
        .find(|(page, _)| *page == name)
        .map(|(page, text)| (*page, *text))
}

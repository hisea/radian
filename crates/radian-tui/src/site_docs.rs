//! The static docs site (`https://getrad.dev/docs/`): renders the bundled user manual
//! to HTML files.
//!
//! The pages come from the same `radian_coding::docs::DOCS` table the binary embeds, so the
//! web manual and `radian docs` can never drift apart. Markdown becomes HTML with
//! `pulldown-cmark` (tables, strikethrough, heading attributes); raw HTML in a page is
//! escaped, never passed through. A page's output path is its name under `docs/user/`
//! (`arc/loop.md` -> `arc/loop.html`), and its asset and page links resolve from that
//! page's own directory. Links between pages (`foo.md`, `../foo.md`, `foo.md#bar`)
//! resolve from the source page's directory, as GitHub renders them, and fail the build
//! when the target page or heading does not exist; relative links out of the manual become
//! `blob/main` URLs. No JavaScript is emitted on any page.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use std::collections::{HashMap, HashSet};

/// The repo URL relative manual links resolve against.
const REPO_BLOB: &str = "https://github.com/hisea/radian/blob/main";

/// The workspace root the generator runs from: the crate lives two levels below it.
const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// Whether `rel` (a workspace-relative path with `/` separators) is a file in the repo.
fn repo_has(rel: &str) -> bool {
    std::path::Path::new(WORKSPACE_ROOT).join(rel).is_file()
}

/// The parser's options: CommonMark plus the extensions the manual uses.
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
}

/// One heading (`##` or deeper) with its stable `id` slug and plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocHeading {
    /// 2 for `##`, 3 for `###`, and so on.
    pub level: u8,
    /// The slug used for `id` and fragment links.
    pub id: String,
    /// The heading's plain text (no markup).
    pub text: String,
}

/// One rendered page.
#[derive(Debug, Clone)]
pub struct RenderedPage {
    /// The file name in `DOCS` (`commands.md`, `arc/loop.md`).
    pub name: String,
    /// The stem (`commands`, `arc/loop`); `index` for the landing page.
    pub stem: String,
    /// The first `# ` heading's plain text.
    pub title: String,
    /// The frontmatter `description:`.
    pub description: String,
    /// The article body HTML.
    pub body: String,
    /// Every `##`+ heading, in order.
    pub headings: Vec<DocHeading>,
    /// Whether the source held raw HTML (escaped in the output).
    pub raw_html: bool,
}

/// One sidebar group: an `index.md` `## ` section and its member pages.
#[derive(Debug, Clone)]
pub struct SiteGroup {
    /// The section heading (`Start`, `Use`, …).
    pub title: String,
    /// Page stems in group order.
    pub stems: Vec<String>,
}

/// The whole rendered site: every page plus the sidebar groups.
#[derive(Debug, Clone)]
pub struct RenderedSite {
    pub pages: Vec<RenderedPage>,
    pub groups: Vec<SiteGroup>,
}

/// A broken link: it names the source page and the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteError {
    /// The source page (`commands.md`).
    pub page: String,
    /// The link destination as written.
    pub link: String,
    /// What is wrong with it.
    pub reason: String,
}

impl std::fmt::Display for SiteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "site-docs: {}: link `{}`: {}",
            self.page, self.link, self.reason
        )
    }
}

impl std::error::Error for SiteError {}

/// Renders every page. Fails on the first broken inter-page or fragment link.
pub fn render_site(pages: &[(&str, &str)]) -> Result<RenderedSite, SiteError> {
    let stems: HashSet<String> = pages
        .iter()
        .map(|(name, _)| stem_of(name).to_owned())
        .collect();
    // Phase one: every page's heading ids, so a fragment link can be checked against a
    // page that renders later.
    let mut ids: HashMap<String, Vec<DocHeading>> = HashMap::new();
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut bodies: HashMap<String, &str> = HashMap::new();
    for (name, text) in pages {
        let stem = stem_of(name);
        let body = strip_frontmatter(text);
        let (title, headings) = collect_headings(body);
        titles.insert(stem.to_owned(), title.unwrap_or_else(|| stem.to_owned()));
        bodies.insert(stem.to_owned(), body);
        ids.insert(stem.to_owned(), headings);
    }
    let mut rendered = Vec::new();
    for (name, text) in pages {
        let stem = stem_of(name);
        let description = radian_coding::docs::description(text).to_owned();
        let body_md = bodies[stem];
        let mut ctx = Ctx {
            page: name,
            stems: &stems,
            ids: &ids,
            raw_html: false,
        };
        let mut cursor = 0usize;
        let body = render_page(body_md, &mut ctx, &ids[stem], &mut cursor)?;
        rendered.push(RenderedPage {
            name: (*name).to_owned(),
            stem: stem.to_owned(),
            title: titles[stem].clone(),
            description,
            body,
            headings: ids[stem].clone(),
            raw_html: ctx.raw_html,
        });
    }
    let groups = sidebar_groups(pages, &stems);
    Ok(RenderedSite {
        pages: rendered,
        groups,
    })
}

/// The full HTML document for one page: header, sidebar, article, "On this page".
pub fn page_html(site: &RenderedSite, index: usize) -> String {
    let page = &site.pages[index];
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1, viewport-fit=cover\">\n",
    );
    out.push_str("<title>");
    escape_into(&page.title, &mut out);
    out.push_str(" · radian docs</title>\n<meta name=\"description\" content=\"");
    escape_into(&page.description, &mut out);
    out.push_str("\">\n<meta property=\"og:title\" content=\"");
    escape_into(&page.title, &mut out);
    out.push_str(" · radian docs\">\n<meta property=\"og:type\" content=\"article\">\n");
    let assets = asset_prefix(&page.stem);
    out.push_str(&format!(
        "<link rel=\"icon\" href=\"{assets}favicon.svg\" type=\"image/svg+xml\">\n"
    ));
    out.push_str("<meta name=\"color-scheme\" content=\"light dark\">\n");
    out.push_str("<link rel=\"preconnect\" href=\"https://fonts.googleapis.com\">\n");
    out.push_str("<link rel=\"preconnect\" href=\"https://fonts.gstatic.com\" crossorigin>\n");
    out.push_str("<link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Geist:wght@400;500;600&family=JetBrains+Mono:wght@400;500;600&display=swap\">\n");
    out.push_str("<link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Sora:wght@600&text=radian&display=swap\">\n");
    out.push_str(&format!(
        "<link rel=\"stylesheet\" href=\"{assets}docs.css\">\n</head>\n<body>\n"
    ));
    out.push_str("<div class=\"sheet\">\n<header class=\"top\">\n<nav aria-label=\"Main\">\n");
    out.push_str("<a class=\"brand lf-sora\" href=\"/\" aria-label=\"radian home\">\n");
    out.push_str("<svg class=\"mark-a\" viewBox=\"1.6 4.4 20.8 18\" aria-hidden=\"true\"><path d=\"M3 21 L21 21 A18 18 0 0 0 12.73 5.85 Z\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\" stroke-linejoin=\"round\"/><path d=\"M21 21 A18 18 0 0 0 12.73 5.85\" fill=\"none\" stroke=\"var(--accent)\" stroke-width=\"2.6\" stroke-linecap=\"round\"/></svg>\n");
    out.push_str("<span>radian</span>\n</a>\n");
    out.push_str("<a class=\"docs-link\" href=\"/docs/\">Docs</a>\n</nav>\n</header>\n");
    out.push_str("<div class=\"wrap\">\n<nav class=\"side\" aria-label=\"Manual\">\n");
    sidebar_into(site, index, &mut out);
    out.push_str("</nav>\n<main class=\"body\">\n<article>\n");
    out.push_str(&page.body);
    out.push_str("</article>\n");
    toc_into(page, &mut out);
    out.push_str("</main>\n</div>\n<footer class=\"foot\">\n<span>getrad.dev</span>\n");
    out.push_str("<a href=\"/\">home</a>\n</footer>\n</div>\n</body>\n</html>\n");
    out
}

/// The sidebar: the landing page, then every group from `index.md`.
fn sidebar_into(site: &RenderedSite, current: usize, out: &mut String) {
    let by_stem: HashMap<&str, usize> = site
        .pages
        .iter()
        .enumerate()
        .map(|(i, p)| (p.stem.as_str(), i))
        .collect();
    if let Some(&home) = by_stem.get("index") {
        sidebar_link(site, home, current, out);
    }
    for group in &site.groups {
        out.push_str("<div class=\"group\">\n<span class=\"g\">");
        escape_into(&group.title, out);
        out.push_str("</span>\n<ul>\n");
        for stem in &group.stems {
            if stem == "index" {
                continue;
            }
            if let Some(&i) = by_stem.get(stem.as_str()) {
                out.push_str("<li>");
                sidebar_link(site, i, current, out);
                out.push_str("</li>\n");
            }
        }
        out.push_str("</ul>\n</div>\n");
    }
}

/// One sidebar link, marked when it is the current page.
fn sidebar_link(site: &RenderedSite, i: usize, current: usize, out: &mut String) {
    let page = &site.pages[i];
    out.push_str("<a href=\"");
    out.push_str(&relative_href(&site.pages[current].stem, &page.stem));
    out.push('"');
    if i == current {
        out.push_str(" aria-current=\"page\"");
    }
    out.push('>');
    escape_into(&page.title, out);
    out.push_str("</a>\n");
}

/// The "On this page" list: the page's `##` headings.
fn toc_into(page: &RenderedPage, out: &mut String) {
    let seconds: Vec<&DocHeading> = page.headings.iter().filter(|h| h.level == 2).collect();
    if seconds.is_empty() {
        return;
    }
    out.push_str("<nav class=\"toc\" aria-label=\"On this page\">\n<span class=\"g\">On this page</span>\n<ul>\n");
    for heading in seconds {
        out.push_str("<li><a href=\"#");
        out.push_str(&heading.id);
        out.push_str("\">");
        escape_into(&heading.text, out);
        out.push_str("</a></li>\n");
    }
    out.push_str("</ul>\n</nav>\n");
}

/// The output file for a page stem: `index.html` for the landing page, `<stem>.html` for
/// every other page (`arc/loop` -> `arc/loop.html`, `arc/index` -> `arc/index.html`).
pub fn output_path(stem: &str) -> String {
    if stem == "index" {
        "index.html".to_owned()
    } else {
        format!("{stem}.html")
    }
}

/// The directory that holds a page's output (`arc` for `arc/loop`), empty at the root.
fn page_dir(stem: &str) -> &str {
    stem.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// `to`'s output path relative to the directory that holds `from`'s page: `gate.html` from
/// `arc/loop` to `arc/gate`, `../configuration.html` from `arc/loop` to `configuration`.
fn relative_href(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = page_dir(from)
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let target = output_path(to);
    let mut parts: Vec<&str> = target.split('/').collect();
    let name = parts.pop().unwrap_or("");
    let common = from_dir
        .iter()
        .zip(parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::new();
    for _ in common..from_dir.len() {
        out.push_str("../");
    }
    for part in &parts[common..] {
        out.push_str(part);
        out.push('/');
    }
    out.push_str(name);
    out
}

/// The `../` prefix that reaches the site root's assets (`docs.css`, `favicon.svg`) from a
/// page: root pages need one, `arc/` pages two.
fn asset_prefix(stem: &str) -> String {
    let depth = page_dir(stem)
        .split('/')
        .filter(|part| !part.is_empty())
        .count();
    "../".repeat(depth + 1)
}

/// The sidebar groups from the landing page: each `## ` section in order with the
/// backticked page stems its list items name. Pages the landing page never names join the
/// last group, so the sidebar still lists exactly the rendered pages.
fn sidebar_groups(pages: &[(&str, &str)], stems: &HashSet<String>) -> Vec<SiteGroup> {
    let index = pages
        .iter()
        .find(|(name, _)| stem_of(name) == "index")
        .map(|(_, text)| strip_frontmatter(text));
    let mut groups: Vec<SiteGroup> = Vec::new();
    if let Some(body) = index {
        for line in body.lines() {
            if let Some(title) = line.strip_prefix("## ") {
                groups.push(SiteGroup {
                    title: title.trim().to_owned(),
                    stems: Vec::new(),
                });
            } else if let Some(stem) = line
                .strip_prefix("- `")
                .and_then(|rest| rest.split('`').next())
                && stems.contains(stem)
                && !groups.iter().any(|g| g.stems.iter().any(|s| s == stem))
                && let Some(group) = groups.last_mut()
            {
                group.stems.push(stem.to_owned());
            }
        }
    }
    // Pages no section names (and every page when the landing page has no sections): the
    // last group takes them, in table order, so none is lost.
    let mut orphans: Vec<String> = Vec::new();
    for (name, _) in pages {
        let stem = stem_of(name);
        if stem != "index" && !groups.iter().any(|g| g.stems.contains(&stem.to_owned())) {
            orphans.push(stem.to_owned());
        }
    }
    if let Some(last) = groups.last_mut() {
        last.stems.extend(orphans);
    }
    groups
}

/// The file stem for a page name (`commands.md` becomes `commands`).
fn stem_of(name: &str) -> &str {
    name.strip_suffix(".md").unwrap_or(name)
}

/// The body after the frontmatter (`---` … `---`); the whole text when it has none.
fn strip_frontmatter(text: &str) -> &str {
    let mut rest = text;
    if let Some(after) = rest.strip_prefix("---") {
        rest = &after[after.find('\n').map_or(after.len(), |i| i + 1)..];
        if let Some(end) = rest.lines().position(|line| line == "---") {
            let skip: usize = rest.lines().take(end + 1).map(|line| line.len() + 1).sum();
            return &rest[skip.min(rest.len())..];
        }
    }
    text
}

/// The first `# ` heading's plain text and every `##`+ heading with its stable id.
fn collect_headings(body: &str) -> (Option<String>, Vec<DocHeading>) {
    let mut title: Option<String> = None;
    let mut headings = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut current: Option<(u8, Option<String>, String)> = None;
    for event in Parser::new_ext(body, options()) {
        match event {
            Event::Start(Tag::Heading { level, id, .. }) => {
                current = Some((
                    level_number(level),
                    id.map(|id| id.into_string()),
                    String::new(),
                ));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, _, buf)) = current.as_mut() {
                    buf.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, buf)) = current.as_mut() {
                    buf.push(' ');
                }
            }
            Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                if let Some((level, explicit, text)) = current.take() {
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if level == 1 {
                        title = Some(text);
                    } else {
                        let base = explicit.unwrap_or_else(|| slugify(&text));
                        let id = unique_id(&base, &mut used);
                        headings.push(DocHeading { level, id, text });
                    }
                }
            }
            _ => {}
        }
    }
    (title, headings)
}

/// The URL slug for a heading: ASCII lowercase, runs of separators become one `-`.
fn slugify(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_' | '\t') && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("section");
    }
    out
}

/// A slug no heading on this page uses yet (`slug`, `slug-1`, …).
fn unique_id(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut n = 1usize;
    loop {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

fn level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// The render state for one page.
struct Ctx<'a> {
    page: &'a str,
    stems: &'a HashSet<String>,
    ids: &'a HashMap<String, Vec<DocHeading>>,
    raw_html: bool,
}

/// Table state while a `<table>` is open.
struct TableState {
    align: Vec<Alignment>,
    in_head: bool,
    column: usize,
}

/// Renders one page body to HTML; `headings` are this page's ids in heading order.
fn render_page(
    body: &str,
    ctx: &mut Ctx<'_>,
    headings: &[DocHeading],
    cursor: &mut usize,
) -> Result<String, SiteError> {
    let events: Vec<Event> = Parser::new_ext(body, options()).collect();
    let mut pos = 0usize;
    let mut tables: Vec<TableState> = Vec::new();
    render_events(&events, &mut pos, ctx, &mut tables, headings, cursor)
}

/// Renders events until the matching `End` (or the stream end), returning the HTML.
fn render_events(
    events: &[Event],
    pos: &mut usize,
    ctx: &mut Ctx<'_>,
    tables: &mut Vec<TableState>,
    headings: &[DocHeading],
    cursor: &mut usize,
) -> Result<String, SiteError> {
    let mut out = String::new();
    while *pos < events.len() {
        match &events[*pos] {
            Event::Start(tag) => {
                *pos += 1;
                render_tag(tag, events, pos, ctx, tables, headings, cursor, &mut out)?;
            }
            Event::End(_) => {
                *pos += 1;
                return Ok(out);
            }
            Event::Text(text) => {
                escape_into(text, &mut out);
                *pos += 1;
            }
            Event::Code(code) => {
                out.push_str("<code>");
                escape_into(code, &mut out);
                out.push_str("</code>");
                *pos += 1;
            }
            Event::SoftBreak => {
                out.push('\n');
                *pos += 1;
            }
            Event::HardBreak => {
                out.push_str("<br>\n");
                *pos += 1;
            }
            Event::Rule => {
                out.push_str("<hr>\n");
                *pos += 1;
            }
            Event::TaskListMarker(checked) => {
                out.push_str("<span class=\"task\">");
                out.push_str(if *checked { "☑" } else { "☐" });
                out.push_str("</span> ");
                *pos += 1;
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                ctx.raw_html = true;
                escape_into(html, &mut out);
                *pos += 1;
            }
            Event::FootnoteReference(label) => {
                escape_into(&format!("[^{label}]"), &mut out);
                *pos += 1;
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                escape_into(math, &mut out);
                *pos += 1;
            }
        }
    }
    Ok(out)
}

/// Renders one `Start` tag: the inner events up to its `End`, wrapped in HTML.
#[allow(clippy::too_many_arguments)]
fn render_tag(
    tag: &Tag,
    events: &[Event],
    pos: &mut usize,
    ctx: &mut Ctx<'_>,
    tables: &mut Vec<TableState>,
    headings: &[DocHeading],
    cursor: &mut usize,
    out: &mut String,
) -> Result<(), SiteError> {
    match tag {
        Tag::Paragraph => {
            out.push_str("<p>");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</p>\n");
        }
        Tag::Heading { level, .. } => {
            let level = level_number(*level);
            let id = if level >= 2 {
                let heading = headings.get(*cursor).ok_or_else(|| SiteError {
                    page: ctx.page.to_owned(),
                    link: String::new(),
                    reason: "more headings rendered than collected".to_owned(),
                })?;
                *cursor += 1;
                Some(heading.id.clone())
            } else {
                None
            };
            out.push_str("<h");
            out.push_str(&level.to_string());
            if let Some(id) = &id {
                out.push_str(" id=\"");
                escape_into(id, out);
                out.push('"');
            }
            out.push('>');
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            if let Some(id) = &id {
                out.push_str("<a class=\"anchor\" href=\"#");
                escape_into(id, out);
                out.push_str("\" aria-label=\"Link to this section\">#</a>");
            }
            out.push_str("</h");
            out.push_str(&level.to_string());
            out.push_str(">\n");
        }
        Tag::BlockQuote(_) => {
            out.push_str("<blockquote>\n");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</blockquote>\n");
        }
        Tag::CodeBlock(kind) => {
            out.push_str("<pre><code");
            if let CodeBlockKind::Fenced(lang) = kind
                && !lang.is_empty()
            {
                out.push_str(" class=\"language-");
                escape_into(&lang_attr(lang), out);
                out.push('"');
            }
            out.push('>');
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</code></pre>\n");
        }
        Tag::HtmlBlock => {
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
        }
        Tag::List(ordered) => {
            out.push_str(if ordered.is_some() {
                "<ol>\n"
            } else {
                "<ul>\n"
            });
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str(if ordered.is_some() {
                "</ol>\n"
            } else {
                "</ul>\n"
            });
        }
        Tag::Item => {
            out.push_str("<li>");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</li>\n");
        }
        Tag::Table(align) => {
            tables.push(TableState {
                align: align.clone(),
                in_head: false,
                column: 0,
            });
            out.push_str("<div class=\"table-wrap\">\n<table>\n");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</table>\n</div>\n");
            tables.pop();
        }
        Tag::TableHead => {
            if let Some(state) = tables.last_mut() {
                state.in_head = true;
            }
            out.push_str("<thead>\n");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</thead>\n");
            if let Some(state) = tables.last_mut() {
                state.in_head = false;
            }
        }
        Tag::TableRow => {
            if let Some(state) = tables.last_mut() {
                state.column = 0;
            }
            out.push_str("<tr>");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</tr>\n");
        }
        Tag::TableCell => {
            let (cell, align) = match tables.last_mut() {
                Some(state) => {
                    let align = state.align.get(state.column).copied();
                    let cell = if state.in_head { "th" } else { "td" };
                    state.column += 1;
                    (cell, align)
                }
                None => ("td", None),
            };
            out.push('<');
            out.push_str(cell);
            match align {
                Some(Alignment::Left) => out.push_str(" align=\"left\""),
                Some(Alignment::Center) => out.push_str(" align=\"center\""),
                Some(Alignment::Right) => out.push_str(" align=\"right\""),
                Some(Alignment::None) | None => {}
            }
            out.push('>');
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</");
            out.push_str(cell);
            out.push_str(">\n");
        }
        Tag::Emphasis => wrap_inline("em", events, pos, ctx, tables, headings, cursor, out)?,
        Tag::Strong => wrap_inline("strong", events, pos, ctx, tables, headings, cursor, out)?,
        Tag::Strikethrough => wrap_inline("del", events, pos, ctx, tables, headings, cursor, out)?,
        Tag::Link { dest_url, .. } => {
            let href = resolve_link(dest_url, ctx)?;
            out.push_str("<a href=\"");
            escape_into(&href, out);
            out.push_str("\">");
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
            out.push_str("</a>");
        }
        Tag::Image { dest_url, .. } => {
            let src = resolve_link(dest_url, ctx)?;
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str("<img src=\"");
            escape_into(&src, out);
            out.push_str("\" alt=\"");
            escape_into(&strip_tags(&inner), out);
            out.push_str("\">");
        }
        // Unreachable with the enabled options: render the content transparently.
        Tag::FootnoteDefinition(_)
        | Tag::DefinitionList
        | Tag::DefinitionListTitle
        | Tag::DefinitionListDefinition
        | Tag::MetadataBlock(_) => {
            let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
            out.push_str(&inner);
        }
    }
    Ok(())
}

/// Renders an inline wrapper (`em`, `strong`, `del`) around its inner events.
#[allow(clippy::too_many_arguments)]
fn wrap_inline(
    tag: &str,
    events: &[Event],
    pos: &mut usize,
    ctx: &mut Ctx<'_>,
    tables: &mut Vec<TableState>,
    headings: &[DocHeading],
    cursor: &mut usize,
    out: &mut String,
) -> Result<(), SiteError> {
    out.push('<');
    out.push_str(tag);
    out.push('>');
    let inner = render_events(events, pos, ctx, tables, headings, cursor)?;
    out.push_str(&inner);
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
    Ok(())
}

/// The language name for a code block's CSS class: alphanumerics, `+` and `-`.
fn lang_attr(lang: &str) -> String {
    let kept: String = lang
        .chars()
        .take_while(|ch| !ch.is_whitespace())
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '+' || *ch == '-')
        .collect();
    if kept.is_empty() {
        "text".to_owned()
    } else {
        kept.to_ascii_lowercase()
    }
}

/// Rewrites one link destination: page links become relative `.html` paths from the source
/// page's directory, repo links become `blob/main` URLs, external links and same-page
/// fragments pass through. Unknown pages and headings fail the build with the source page
/// and the link.
fn resolve_link(dest: &str, ctx: &mut Ctx<'_>) -> Result<String, SiteError> {
    let dest = dest.trim();
    let (path, fragment) = match dest.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (dest, None),
    };
    if path.is_empty() {
        // A same-page fragment (`#setup`): it must name a heading on this page.
        let id = fragment.unwrap_or("");
        if id.is_empty() {
            return Err(SiteError {
                page: ctx.page.to_owned(),
                link: dest.to_owned(),
                reason: "empty links resolve nowhere".to_owned(),
            });
        }
        check_fragment(ctx.page, id, ctx)?;
        return Ok(dest.to_owned());
    }
    if has_scheme(path) {
        return Ok(dest.to_owned());
    }
    let source = stem_of(ctx.page);
    let source_dir = page_dir(source);
    // A page link is written relative to its source page, as GitHub renders it
    // (`gate.md` and `../configuration.md` from `arc/loop.md`).
    if path.ends_with(".md")
        && let Some(resolved) = join_docs_user(source_dir, path)
        && let Some(stem) = resolved.strip_suffix(".md")
        && ctx.stems.contains(stem)
    {
        if let Some(id) = fragment
            && !id.is_empty()
            && !ctx
                .ids
                .get(stem)
                .is_some_and(|headings| headings.iter().any(|h| h.id == id))
        {
            return Err(SiteError {
                page: ctx.page.to_owned(),
                link: dest.to_owned(),
                reason: format!("`{path}` has no heading `#{id}`"),
            });
        }
        let mut href = relative_href(source, stem);
        if let Some(id) = fragment {
            href.push('#');
            href.push_str(id);
        }
        return Ok(href);
    }
    // A path under `docs/user/` that is not in the table is a missing page, never a repo
    // link: `missing.md` from `arc/loop.md` names `docs/user/arc/missing.md`.
    if path.ends_with(".md")
        && let Some(resolved) = join_docs_user(source_dir, path)
        && repo_has(&format!("docs/user/{resolved}"))
    {
        let mut href = format!("{REPO_BLOB}/docs/user/{resolved}");
        if let Some(id) = fragment {
            href.push('#');
            href.push_str(id);
        }
        return Ok(href);
    }
    if path.contains('/') || path.starts_with('.') {
        // A link out of the manual (`../adr/…`): resolve it from the source page's directory
        // and point at the file on `main`.
        let mut href = format!("{REPO_BLOB}/{}", repo_path(path, source_dir));
        if let Some(id) = fragment {
            href.push('#');
            href.push_str(id);
        }
        return Ok(href);
    }
    // A bare name (`ARCHITECTURE.md`): a repo file when it exists, else a link to a
    // page that is not in the table, which fails the build.
    for base in ["docs/user", ""] {
        let rel = if base.is_empty() {
            path.to_owned()
        } else {
            format!("{base}/{path}")
        };
        if repo_has(&rel) {
            let mut href = format!("{REPO_BLOB}/{rel}");
            if let Some(id) = fragment {
                href.push('#');
                href.push_str(id);
            }
            return Ok(href);
        }
    }
    Err(SiteError {
        page: ctx.page.to_owned(),
        link: dest.to_owned(),
        reason: "no manual page with that name".to_owned(),
    })
}

/// Whether `path` carries a URL scheme (`https:`, `mailto:`, …).
fn has_scheme(path: &str) -> bool {
    let Some(colon) = path.find(':') else {
        return false;
    };
    let scheme = &path[..colon];
    !scheme.is_empty()
        && scheme.chars().enumerate().all(|(i, ch)| {
            ch.is_ascii_alphabetic() || (i > 0 && (ch.is_ascii_digit() || "+-.".contains(ch)))
        })
        && !scheme.contains('/')
}

/// Resolves a link relative to `source_dir` (the page's directory under `docs/user/`) to a
/// normalized path under `docs/user/`, or `None` when the link leaves that directory.
fn join_docs_user(source_dir: &str, path: &str) -> Option<String> {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        source_dir
            .split('/')
            .filter(|part| !part.is_empty())
            .collect()
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            rest => parts.push(rest),
        }
    }
    Some(parts.join("/"))
}

/// Resolves a repo-relative link from `source_dir` (the page's directory under `docs/user/`)
/// to a repo path (a leading `/` counts from the repo root instead), collapsing `.` and `..`.
fn repo_path(path: &str, source_dir: &str) -> String {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        let mut base = vec!["docs", "user"];
        base.extend(source_dir.split('/').filter(|part| !part.is_empty()));
        base
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            rest => parts.push(rest),
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.join("/")
}

/// Whether `id` is a heading on `page`.
fn check_fragment(page: &str, id: &str, ctx: &mut Ctx<'_>) -> Result<(), SiteError> {
    let stem = stem_of(page);
    let known = ctx
        .ids
        .get(stem)
        .is_some_and(|headings| headings.iter().any(|h| h.id == id));
    if known {
        Ok(())
    } else {
        Err(SiteError {
            page: ctx.page.to_owned(),
            link: format!("#{id}"),
            reason: "no heading with that id on this page".to_owned(),
        })
    }
}

/// Plain text without tags (image `alt` text).
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// HTML-escapes `text` into `out`.
fn escape_into(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders one synthetic page alongside an optional second page it may link to.
    fn render_two(main: &str, other: Option<(&str, &str)>) -> Result<RenderedSite, SiteError> {
        let mut pages = vec![("a.md", main)];
        if let Some((name, text)) = other {
            pages.push((name, text));
        }
        render_site(&pages)
    }

    #[test]
    fn page_links_become_html() {
        let site = render_two(
            "# A\n\nSee [the other page](b.md) and [its part](b.md#deep).\n",
            Some(("b.md", "# B\n\n## Deep\n")),
        )
        .expect("links resolve");
        assert!(
            site.pages[0].body.contains("href=\"b.html\""),
            "{}",
            site.pages[0].body
        );
        assert!(
            site.pages[0].body.contains("href=\"b.html#deep\""),
            "{}",
            site.pages[0].body
        );
    }

    #[test]
    fn an_unknown_page_fails_naming_source_and_link() {
        let error = render_two("# A\n\nSee [gone](missing.md).\n", None).expect_err("must fail");
        assert_eq!(error.page, "a.md");
        assert_eq!(error.link, "missing.md");
    }

    #[test]
    fn an_unknown_fragment_fails() {
        let error = render_two(
            "# A\n\nSee [elsewhere](b.md#nope).\n",
            Some(("b.md", "# B\n\n## Here\n")),
        )
        .expect_err("must fail");
        assert_eq!(error.page, "a.md");
        assert!(error.reason.contains("#nope"), "{}", error.reason);
    }

    #[test]
    fn a_same_page_fragment_must_exist() {
        let error =
            render_two("# A\n\n## Here\n\nSee [below](#gone).\n", None).expect_err("must fail");
        assert_eq!(error.link, "#gone");
        let site = render_two("# A\n\n## Here\n\nSee [below](#here).\n", None)
            .expect("the fragment exists");
        assert!(site.pages[0].body.contains("href=\"#here\""));
    }

    #[test]
    fn repo_links_point_at_main() {
        let site =
            render_two("# A\n\nSee [the adr](../adr/0001-x.md).\n", None).expect("repo links pass");
        assert!(
            site.pages[0]
                .body
                .contains("href=\"https://github.com/hisea/radian/blob/main/docs/adr/0001-x.md\""),
            "{}",
            site.pages[0].body
        );
        // A bare repo file name is a repo link too, not a missing page.
        let site = render_two("# A\n\nSee [the design](ARCHITECTURE.md).\n", None)
            .expect("repo root files pass");
        assert!(
            site.pages[0]
                .body
                .contains("href=\"https://github.com/hisea/radian/blob/main/ARCHITECTURE.md\""),
            "{}",
            site.pages[0].body
        );
    }

    #[test]
    fn raw_html_is_escaped_and_reported() {
        let site = render_two("# A\n\nA <widget> here.\n", None).expect("renders");
        assert!(site.pages[0].raw_html);
        assert!(
            site.pages[0].body.contains("&lt;widget&gt;"),
            "{}",
            site.pages[0].body
        );
        assert!(!site.pages[0].body.contains("<widget>"));
    }

    #[test]
    fn headings_get_stable_ids_and_toc_lists_h2() {
        let site = render_two(
            "# A\n\n## Setup now!\n\n### Deep dive\n\n## Setup now!\n",
            None,
        )
        .expect("renders");
        let ids: Vec<&str> = site.pages[0]
            .headings
            .iter()
            .map(|h| h.id.as_str())
            .collect();
        assert_eq!(ids, ["setup-now", "deep-dive", "setup-now-1"]);
        assert!(site.pages[0].body.contains("<h2 id=\"setup-now\">"));
        assert!(
            site.pages[0]
                .body
                .contains("<a class=\"anchor\" href=\"#setup-now\"")
        );
        let html = page_html(&site, 0);
        assert!(html.contains("<nav class=\"toc\""), "{html}");
        assert_eq!(html.matches("<li><a href=\"#").count(), 2, "{html}");
    }

    #[test]
    fn code_blocks_escape_and_tables_scroll() {
        let site = render_two(
            "# A\n\n```rust\nlet x = 1 < 2;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
            None,
        )
        .expect("renders");
        assert!(
            site.pages[0].body.contains("let x = 1 &lt; 2;"),
            "{}",
            site.pages[0].body
        );
        assert!(
            site.pages[0].body.contains("<div class=\"table-wrap\">"),
            "{}",
            site.pages[0].body
        );
    }
}

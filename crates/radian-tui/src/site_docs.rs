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
//! `blob/main` URLs. No JavaScript is emitted on any page: the one `<script>` element is
//! the `application/ld+json` `TechArticle` record a search engine reads, never run code.
//!
//! The same table also gives three files at the site root: `llms.txt` (one line per page
//! with its description, in sidebar order), `llms-full.txt` (every page's Markdown in one
//! file, each under its own URL) and `sitemap.xml` (every page's URL). Every page names its
//! own absolute URL in a `canonical` link.
//!
//! The two hand-written pages (`site/index.html`, `site/arc.html`) hold English and Chinese in
//! one source; `split_page` writes the English page and the Chinese page (`/zh/`) from each.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use std::collections::{HashMap, HashSet};

/// The repo URL relative manual links resolve against.
const REPO_BLOB: &str = "https://github.com/hisea/radian/blob/main";

/// The repository the site links as the source.
const REPO_URL: &str = "https://github.com/hisea/radian";

/// The site origin, without a trailing slash.
pub const SITE_URL: &str = "https://getrad.dev";

/// A page language of the site: the closed set of languages the hand-written pages are
/// split into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// Every language, English first.
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Zh];

    /// The `lang` attribute and `hreflang` value.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh-Hans",
        }
    }

    /// The class name the source pages give this language's elements.
    fn class(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    fn other(self) -> Lang {
        match self {
            Lang::En => Lang::Zh,
            Lang::Zh => Lang::En,
        }
    }
}

/// One output page of a hand-written source file: the file under `site/` it comes from,
/// its language, its path under the origin, and the title and one-line description
/// `llms.txt` prints.
#[derive(Debug, Clone, Copy)]
pub struct HandPage {
    pub file: &'static str,
    pub lang: Lang,
    pub path: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

/// The site's hand-written pages: each bilingual source file gives one page per language.
/// `splitter`, `llms.txt` and `sitemap.xml` all read this table. The home page is
/// `index.html`, served at `/` and `/zh/`.
pub const HAND_WRITTEN_PAGES: [HandPage; 4] = [
    HandPage {
        file: "index.html",
        lang: Lang::En,
        path: "/",
        title: "radian",
        description: "A fast, efficient coding agent in one Rust binary: a full-screen terminal interface, a headless `radian exec` command, a journalled session file, sandboxed tools, MCP, hooks, skills and memory.",
    },
    HandPage {
        file: "arc.html",
        lang: Lang::En,
        path: "/arc.html",
        title: "arc",
        description: "The plan and task orchestrator built into the radian binary: it runs each task as its own headless radian worker and merges only what passes a fixed gate.",
    },
    HandPage {
        file: "index.html",
        lang: Lang::Zh,
        path: "/zh/",
        title: "radian（中文）",
        description: "radian 是一个快速、高效的 Rust 单文件编码 agent：全屏终端界面、无头的 `radian exec` 命令、可回放的会话文件、沙箱工具、MCP、hooks、skills 和记忆。",
    },
    HandPage {
        file: "arc.html",
        lang: Lang::Zh,
        path: "/zh/arc.html",
        title: "arc（中文）",
        description: "内置在 radian 二进制里的计划与任务编排器：每个任务交给独立的无头 radian worker 运行，只合并通过固定检查关卡的结果。",
    },
];

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
    /// The page's Markdown after the frontmatter, as `llms-full.txt` prints it.
    pub source: String,
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
            source: body_md.to_owned(),
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
    out.push_str(&format!(
        "<link rel=\"canonical\" href=\"{}\">\n",
        page_url(&page.stem)
    ));
    out.push_str("<script type=\"application/ld+json\">");
    out.push_str(&json_ld(page));
    out.push_str("</script>\n");
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

/// The page's absolute URL: `https://getrad.dev/docs/` for the landing page, the directory
/// URL for a subdirectory index (`docs/arc/`), `https://getrad.dev/docs/<stem>.html` otherwise.
pub fn page_url(stem: &str) -> String {
    if stem == "index" {
        format!("{SITE_URL}/docs/")
    } else if let Some(dir) = stem.strip_suffix("/index") {
        format!("{SITE_URL}/docs/{dir}/")
    } else {
        format!("{SITE_URL}/docs/{stem}.html")
    }
}

/// The page's Schema.org `TechArticle` record, serialised for a
/// `<script type="application/ld+json">` element. `</` is written as `<\/` so no title or
/// description can close the element early.
fn json_ld(page: &RenderedPage) -> String {
    let record = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "TechArticle",
        "headline": page.title,
        "description": page.description,
        "url": page_url(&page.stem),
        "inLanguage": "en",
        "isPartOf": {
            "@type": "WebSite",
            "name": "radian",
            "url": format!("{SITE_URL}/"),
        },
        "about": {
            "@type": "SoftwareApplication",
            "name": "radian",
            "url": format!("{SITE_URL}/"),
            "sameAs": REPO_URL,
        },
    });
    record.to_string().replace("</", "<\\/")
}

/// The `llms.txt` index: the hand-written pages, then every manual page in sidebar order
/// with its description, then the full-text file and the repository.
pub fn llms_txt(site: &RenderedSite) -> String {
    let mut out = String::new();
    out.push_str("# radian\n\n");
    out.push_str(
        "> radian is a coding agent in one Rust binary. This file lists every page of getrad.dev; ",
    );
    out.push_str("llms-full.txt holds the whole user manual as Markdown.\n\n");
    out.push_str("## Site\n\n");
    for page in HAND_WRITTEN_PAGES {
        let HandPage {
            path,
            title,
            description,
            ..
        } = page;
        out.push_str(&format!("- [{title}]({SITE_URL}{path}): {description}\n"));
    }
    let by_stem: HashMap<&str, &RenderedPage> =
        site.pages.iter().map(|p| (p.stem.as_str(), p)).collect();
    out.push_str("\n## Manual\n\n");
    if let Some(home) = by_stem.get("index") {
        llms_line(home, &mut out);
    }
    for group in &site.groups {
        out.push_str(&format!("\n### {}\n\n", group.title));
        for stem in &group.stems {
            if stem == "index" {
                continue;
            }
            if let Some(page) = by_stem.get(stem.as_str()) {
                llms_line(page, &mut out);
            }
        }
    }
    out.push_str("\n## Optional\n\n");
    out.push_str(&format!(
        "- [Full manual]({SITE_URL}/llms-full.txt): every manual page in one Markdown file\n"
    ));
    out.push_str(&format!(
        "- [Source]({REPO_URL}): the repository, MIT or Apache-2.0\n"
    ));
    out
}

/// The `sitemap.xml` file: the hand-written pages, then every manual page, each as one
/// `<url><loc>` entry.
pub fn sitemap_xml(site: &RenderedSite) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for page in HAND_WRITTEN_PAGES {
        let path = page.path;
        out.push_str(&format!("<url><loc>{SITE_URL}{path}</loc></url>\n"));
    }
    for page in &site.pages {
        out.push_str(&format!("<url><loc>{}</loc></url>\n", page_url(&page.stem)));
    }
    out.push_str("</urlset>\n");
    out
}

/// One `llms.txt` line: the title, the URL and the description.
fn llms_line(page: &RenderedPage, out: &mut String) {
    out.push_str(&format!("- [{}]({})", page.title, page_url(&page.stem)));
    if !page.description.is_empty() {
        out.push_str(": ");
        out.push_str(&page.description);
    }
    out.push('\n');
}

/// The `llms-full.txt` file: every manual page's Markdown, in table order, each after a
/// line naming its URL and separated by a rule.
pub fn llms_full_txt(site: &RenderedSite) -> String {
    let mut out = String::new();
    out.push_str("# radian manual\n\n");
    out.push_str(&format!(
        "The whole user manual of radian ({SITE_URL}/docs/) in one file. Each page starts with its URL.\n"
    ));
    for page in &site.pages {
        out.push_str("\n---\n\n");
        out.push_str(&format!("URL: {}\n\n", page_url(&page.stem)));
        out.push_str(page.source.trim_matches('\n'));
        out.push('\n');
    }
    out
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

/// A hand-written source page that cannot be split: it names the file and the missing or
/// broken marker.
pub fn split_error(file: &str, marker: &str, reason: &str) -> SiteError {
    SiteError {
        page: format!("site/{file}"),
        link: marker.to_owned(),
        reason: reason.to_owned(),
    }
}

fn hand_page(file: &str, lang: Lang) -> Result<&'static HandPage, SiteError> {
    HAND_WRITTEN_PAGES
        .iter()
        .find(|page| page.file == file && page.lang == lang)
        .ok_or_else(|| split_error(file, file, "is not a hand-written page"))
}

/// The absolute URL of a hand-written page in a language.
pub fn hand_page_url(file: &str, lang: Lang) -> Result<String, SiteError> {
    Ok(format!("{SITE_URL}{}", hand_page(file, lang)?.path))
}

/// Splits one bilingual source page (`site/index.html`, `site/arc.html`) into the page for
/// `lang`: the other language's elements are removed, the language toggle becomes two links
/// to the two pages, the inline language script goes, and the head carries this page's
/// title, description, canonical, `hreflang` alternates and JSON-LD. A missing marker
/// fails with the file's name rather than writing a half page.
pub fn split_page(source: &str, file: &str, lang: Lang) -> Result<String, SiteError> {
    let page = hand_page(file, lang)?;
    let url = format!("{SITE_URL}{}", page.path);
    let mut html = filter_language(source, file, lang)?;

    // The head text: the Chinese strings sit in two metas of the source.
    let title_zh = meta_value(&html, "name=\"title:zh\"", file)?;
    let description_zh = meta_value(&html, "name=\"description:zh\"", file)?;
    html = remove_line_containing(&html, "name=\"title:zh\"");
    html = remove_line_containing(&html, "name=\"description:zh\"");
    let description_en = meta_value(&html, "name=\"description\"", file)?;
    let description = if lang == Lang::Zh {
        set_meta(&mut html, "name=\"description\"", &description_zh, file)?;
        set_meta(
            &mut html,
            "property=\"og:description\"",
            &description_zh,
            file,
        )?;
        set_meta(&mut html, "property=\"og:title\"", &title_zh, file)?;
        replace_between(&mut html, "<title>", "</title>", &title_zh, file)?;
        description_zh
    } else {
        description_en
    };

    html = language_css(&html, lang);
    let html_tag = format!("<html lang=\"{}\">", lang.code());
    if !html.contains("<html lang=\"en\">") {
        return Err(split_error(file, "<html lang=\"en\">", "is missing"));
    }
    html = html.replacen("<html lang=\"en\">", &html_tag, 1);
    html = drop_language_script(&html, file)?;

    // Relative URLs: the Chinese pages live one directory down.
    if lang == Lang::Zh {
        for (from, to) in [
            ("href=\"favicon.svg\"", "href=\"/favicon.svg\""),
            ("=\"media/", "=\"/media/"),
            ("href=\"/arc.html\"", "href=\"/zh/arc.html\""),
            ("href=\"/#", "href=\"/zh/#"),
            ("href=\"/\"", "href=\"/zh/\""),
            ("media/arc-en.jpg\">", "media/arc-zh.jpg\">"),
        ] {
            html = html.replace(from, to);
        }
    }

    set_url_metas(&mut html, &url, file)?;
    html = json_ld_for(&html, &url, &description, lang, file)?;
    html = language_toggle(&html, file, lang)?;

    for banned in ["data-lang", "rad-lang", "localStorage", "setLang"] {
        if html.contains(banned) {
            return Err(split_error(file, banned, "is left in the split page"));
        }
    }
    Ok(html)
}

/// Removes the other language's elements and the language class from the kept ones.
fn filter_language(source: &str, file: &str, keep: Lang) -> Result<String, SiteError> {
    let drop = keep.other().class();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < source.len() {
        let Some(rel) = source[i..].find('<') else {
            out.push_str(&source[i..]);
            break;
        };
        out.push_str(&source[i..i + rel]);
        i += rel;
        let rest = &source[i..];
        if rest.starts_with("<!--") {
            let end = rest
                .find("-->")
                .ok_or_else(|| split_error(file, "<!--", "comment is not closed"))?;
            out.push_str(&rest[..end + 3]);
            i += end + 3;
            continue;
        }
        let name: String = rest[1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if name.is_empty() {
            out.push('<');
            i += 1;
            continue;
        }
        let tag_len = tag_end(rest)
            .ok_or_else(|| split_error(file, &format!("<{name}"), "tag is not closed"))?;
        let tag = &rest[..tag_len];
        if (name == "script" || name == "style") && !tag.ends_with("/>") {
            let close = format!("</{name}");
            let end = rest[tag_len..]
                .find(&close)
                .ok_or_else(|| split_error(file, &format!("<{name}"), "is not closed"))?;
            let stop = rest[tag_len + end..]
                .find('>')
                .map(|n| tag_len + end + n + 1);
            let stop = stop.ok_or_else(|| split_error(file, &close, "is not closed"))?;
            out.push_str(&rest[..stop]);
            i += stop;
            continue;
        }
        let tokens = class_tokens(tag);
        let marked = tokens.iter().any(|t| *t == "en" || *t == "zh");
        if marked && tokens.contains(&drop) {
            i += element_len(rest, &name, tag_len, file)?;
            continue;
        }
        if marked {
            out.push_str(&without_language_class(tag));
        } else {
            out.push_str(tag);
        }
        i += tag_len;
    }
    Ok(out)
}

/// The byte length of the start tag at the start of `s`, quotes respected.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (index, ch) in s.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => return Some(index + 1),
            _ => {}
        }
    }
    None
}

/// The class tokens of a start tag.
fn class_tokens(tag: &str) -> Vec<&str> {
    let Some(start) = tag.find(" class=\"") else {
        return Vec::new();
    };
    let value = &tag[start + 8..];
    value
        .split('"')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect()
}

/// The start tag with `en` and `zh` removed from its class list (the attribute goes when
/// nothing else is left).
fn without_language_class(tag: &str) -> String {
    let Some(start) = tag.find(" class=\"") else {
        return tag.to_owned();
    };
    let value_start = start + 8;
    let value_end = value_start + tag[value_start..].find('"').unwrap_or(0);
    let kept: Vec<&str> = tag[value_start..value_end]
        .split_whitespace()
        .filter(|t| *t != "en" && *t != "zh")
        .collect();
    let attr = if kept.is_empty() {
        String::new()
    } else {
        format!(" class=\"{}\"", kept.join(" "))
    };
    format!("{}{}{}", &tag[..start], attr, &tag[value_end + 1..])
}

/// The byte length of the whole element whose start tag is `tag_len` bytes long, found by
/// counting same-name tags. A void element is its start tag.
fn element_len(rest: &str, name: &str, tag_len: usize, file: &str) -> Result<usize, SiteError> {
    const VOID: [&str; 8] = ["img", "source", "br", "hr", "meta", "link", "input", "wbr"];
    if VOID.contains(&name) || rest[..tag_len].ends_with("/>") {
        return Ok(tag_len);
    }
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut depth = 1;
    let mut pos = tag_len;
    while let Some(rel) = rest[pos..].find('<') {
        let at = pos + rel;
        let here = &rest[at..];
        let boundary = |n: usize| {
            here[n..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-'))
        };
        if here.starts_with(&close) && boundary(close.len()) {
            depth -= 1;
            let end = here
                .find('>')
                .ok_or_else(|| split_error(file, &close, "is not closed"))?;
            if depth == 0 {
                return Ok(at + end + 1);
            }
            pos = at + end + 1;
        } else if here.starts_with(&open) && boundary(open.len()) {
            let len = tag_end(here).ok_or_else(|| split_error(file, &open, "tag is not closed"))?;
            if !here[..len].ends_with("/>") {
                depth += 1;
            }
            pos = at + len;
        } else {
            pos = at + 1;
        }
    }
    Err(split_error(
        file,
        &format!("<{name}"),
        "element is not closed",
    ))
}

/// The `content` of the `<meta …>` whose attribute text contains `key`.
fn meta_value(html: &str, key: &str, file: &str) -> Result<String, SiteError> {
    let marker = format!("<meta {key} content=\"");
    let start = html
        .find(&marker)
        .ok_or_else(|| split_error(file, &format!("<meta {key}>"), "is missing"))?
        + marker.len();
    let end = html[start..]
        .find('"')
        .ok_or_else(|| split_error(file, &format!("<meta {key}>"), "is not closed"))?;
    Ok(html[start..start + end].to_owned())
}

fn set_meta(html: &mut String, key: &str, value: &str, file: &str) -> Result<(), SiteError> {
    let marker = format!("<meta {key} content=\"");
    replace_between(html, &marker, "\"", value, file)
}

/// Replaces the text between the first `open` and the next `close` after it.
fn replace_between(
    html: &mut String,
    open: &str,
    close: &str,
    value: &str,
    file: &str,
) -> Result<(), SiteError> {
    let start = html
        .find(open)
        .ok_or_else(|| split_error(file, open, "is missing"))?
        + open.len();
    let end = html[start..]
        .find(close)
        .ok_or_else(|| split_error(file, open, "is not closed"))?;
    html.replace_range(start..start + end, value);
    Ok(())
}

fn remove_line_containing(html: &str, needle: &str) -> String {
    html.split_inclusive('\n')
        .filter(|line| !line.contains(needle))
        .collect()
}

/// Drops the source's language rules; for Chinese the typography rules apply unconditionally.
/// The toggle's links look like the buttons they replace.
fn language_css(html: &str, lang: Lang) -> String {
    const ZH_RULE: &str = ":root[data-lang=\"zh\"] ";
    let mut out = String::with_capacity(html.len());
    for line in html.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if trimmed.starts_with(":root:not([data-lang=\"zh\"])") {
            continue;
        }
        if let Some(rule) = trimmed.strip_prefix(ZH_RULE) {
            if lang == Lang::Zh {
                out.push_str(indent);
                out.push_str(rule);
            }
            continue;
        }
        if trimmed.starts_with(".lang button {") {
            out.push_str(&line.replace(".lang button {", ".lang a {").replace(
                "cursor: pointer; }",
                "cursor: pointer; text-decoration: none; }",
            ));
        } else if trimmed.starts_with(".lang button[aria-pressed=\"true\"]") {
            out.push_str(&line.replace(
                ".lang button[aria-pressed=\"true\"]",
                ".lang a[aria-current=\"page\"]",
            ));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Removes the language part of the inline script (state, storage, the toggle's handlers);
/// the copy-button handlers stay.
fn drop_language_script(html: &str, file: &str) -> Result<String, SiteError> {
    let start = html
        .find("  var root = document.documentElement;")
        .ok_or_else(|| split_error(file, "var root = document.documentElement;", "is missing"))?;
    let stop = html
        .find("  document.querySelectorAll('.install-row")
        .ok_or_else(|| split_error(file, ".install-row", "script block is missing"))?;
    if stop < start {
        return Err(split_error(
            file,
            ".install-row",
            "script block is out of order",
        ));
    }
    Ok(format!("{}{}", &html[..start], &html[stop..]))
}

/// Points `canonical` and `og:url` at the page's own URL and lists the three alternates.
fn set_url_metas(html: &mut String, url: &str, file: &str) -> Result<(), SiteError> {
    set_meta(html, "property=\"og:url\"", url, file)?;
    let canonical = "<link rel=\"canonical\" href=\"";
    replace_between(html, canonical, "\"", url, file)?;
    let en = hand_page_url(file, Lang::En)?;
    let zh = hand_page_url(file, Lang::Zh)?;
    let start = html.find(canonical).unwrap_or(0);
    let line_end = start + html[start..].find('\n').unwrap_or(0) + 1;
    let alternates = format!(
        "<link rel=\"alternate\" hreflang=\"en\" href=\"{en}\">\n<link rel=\"alternate\" hreflang=\"zh-Hans\" href=\"{zh}\">\n<link rel=\"alternate\" hreflang=\"x-default\" href=\"{en}\">\n"
    );
    html.insert_str(line_end, &alternates);
    Ok(())
}

/// Rewrites the JSON-LD record: this page's URL, description and language.
fn json_ld_for(
    html: &str,
    url: &str,
    description: &str,
    lang: Lang,
    file: &str,
) -> Result<String, SiteError> {
    let open = "<script type=\"application/ld+json\">";
    let start = html
        .find(open)
        .ok_or_else(|| split_error(file, open, "is missing"))?
        + open.len();
    let end = html[start..]
        .find("</script>")
        .ok_or_else(|| split_error(file, open, "is not closed"))?;
    let mut record: serde_json::Value = serde_json::from_str(&html[start..start + end])
        .map_err(|e| split_error(file, "ld+json", &format!("does not parse: {e}")))?;
    let object = record
        .as_object_mut()
        .ok_or_else(|| split_error(file, "ld+json", "is not an object"))?;
    object.insert("url".into(), url.into());
    object.insert("description".into(), unescape(description).into());
    object.insert("inLanguage".into(), lang.code().into());
    let text = record.to_string().replace("</", "<\\/");
    Ok(format!(
        "{}{}{}",
        &html[..start],
        text,
        &html[start + end..]
    ))
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Replaces the language buttons with two links to the page in each language.
fn language_toggle(html: &str, file: &str, lang: Lang) -> Result<String, SiteError> {
    let open = "<div class=\"lang\"";
    let start = html
        .find(open)
        .ok_or_else(|| split_error(file, open, "language toggle is missing"))?;
    let end = html[start..]
        .find("</div>")
        .ok_or_else(|| split_error(file, open, "language toggle is not closed"))?
        + start
        + "</div>".len();
    let line_start = html[..start].rfind('\n').map_or(0, |n| n + 1);
    let indent = &html[line_start..start];
    let link = |target: Lang, text: &str| -> Result<String, SiteError> {
        let path = hand_page(file, target)?.path;
        let current = if target == lang {
            " aria-current=\"page\""
        } else {
            ""
        };
        Ok(format!(
            "{indent}  <a href=\"{path}\" hreflang=\"{code}\" lang=\"{code}\"{current}>{text}</a>\n",
            code = target.code()
        ))
    };
    let toggle = format!(
        "<div class=\"lang\" role=\"group\" aria-label=\"Language\">\n{}{}{indent}</div>",
        link(Lang::En, "EN")?,
        link(Lang::Zh, "中")?
    );
    Ok(format!("{}{}{}", &html[..start], toggle, &html[end..]))
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

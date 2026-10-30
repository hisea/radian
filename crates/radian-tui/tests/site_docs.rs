//! The web manual (`site-docs` example) renders every `DOCS` page, resolves every
//! inter-page link and fragment, lists exactly the manual pages in its sidebar, emits no
//! JavaScript (only a `TechArticle` JSON-LD record), names its canonical URL on every page,
//! and lists every page once in `llms.txt`, `sitemap.xml` and in full in `llms-full.txt`.

use radian_tui::site_docs::{RenderedSite, render_site};
use std::collections::HashSet;

/// The hand-written site pages that link into the generated docs, with the canonical URL
/// each must name (`None` for the 404 page, which is `noindex` instead).
const HAND_WRITTEN_PAGES: [(&str, Option<&str>); 3] = [
    ("index.html", Some("https://getrad.dev/")),
    ("arc.html", Some("https://getrad.dev/arc.html")),
    ("404.html", None),
];

fn site() -> RenderedSite {
    render_site(radian_coding::docs::DOCS).expect("the manual renders without broken links")
}

/// Every `DOCS` page renders with a title, a description and an `h1`.
#[test]
fn every_docs_page_renders() {
    let site = site();
    assert_eq!(
        site.pages.len(),
        radian_coding::docs::DOCS.len(),
        "one rendered page per DOCS page"
    );
    for page in &site.pages {
        assert!(!page.title.is_empty(), "{} has a title", page.name);
        assert!(
            !page.description.is_empty(),
            "{} has a description",
            page.name
        );
        assert!(
            page.body.contains("<h1>"),
            "{} keeps its top heading: {}",
            page.name,
            page.body.chars().take(120).collect::<String>()
        );
    }
}

/// Every `href` in every page points at a generated page and, with a fragment, at an
/// existing heading `id` on that page.
#[test]
fn every_link_resolves_to_a_page_and_heading() {
    let site = site();
    let by_stem: std::collections::HashMap<&str, &radian_tui::site_docs::RenderedPage> =
        site.pages.iter().map(|p| (p.stem.as_str(), p)).collect();
    for page in &site.pages {
        for href in hrefs(&page.body) {
            if href.starts_with("https://") || href.starts_with("http://") {
                continue;
            }
            let (path, fragment) = match href.split_once('#') {
                Some((path, fragment)) => (path, Some(fragment)),
                None => (href, None),
            };
            let target = if path.is_empty() {
                page
            } else {
                let stem = resolve_href(&page.stem, path)
                    .and_then(|resolved| resolved.strip_suffix(".html").map(str::to_owned))
                    .unwrap_or_else(|| {
                        panic!("{}: link `{href}` points outside the docs", page.name)
                    });
                by_stem
                    .get(stem.as_str())
                    .unwrap_or_else(|| panic!("{}: link `{href}` has no page", page.name))
            };
            if let Some(id) = fragment
                && !id.is_empty()
                && !target.headings.iter().any(|h| h.id == id)
            {
                panic!("{}: link `{href}` has no heading `#{id}`", page.name);
            }
        }
    }
}

/// The sidebar lists exactly the `DOCS` pages: the landing page plus every grouped page.
#[test]
fn the_sidebar_lists_exactly_the_docs_pages() {
    let site = site();
    let expected: HashSet<String> = radian_coding::docs::DOCS
        .iter()
        .map(|(name, _)| {
            let stem = name.strip_suffix(".md").unwrap_or(name);
            if stem == "index" {
                "index.html".to_owned()
            } else {
                format!("{stem}.html")
            }
        })
        .collect();
    for (index, page) in site.pages.iter().enumerate() {
        let html = radian_tui::site_docs::page_html(&site, index);
        let sidebar = html
            .split_once("<nav class=\"side\"")
            .expect("a sidebar")
            .1
            .split_once("</nav>")
            .expect("the sidebar ends")
            .0;
        let listed: HashSet<String> = hrefs(sidebar)
            .into_iter()
            .map(|href| {
                resolve_href(&page.stem, href).unwrap_or_else(|| {
                    panic!("{}: sidebar link `{href}` leaves the site", page.name)
                })
            })
            .collect();
        let expected_ref: HashSet<&str> = expected.iter().map(String::as_str).collect();
        // The sidebar holds pages only.
        assert_eq!(
            listed,
            expected_ref.iter().map(|href| (*href).to_owned()).collect(),
            "{} sidebar lists exactly the manual pages",
            page.name
        );
    }
}

/// No docs page emits JavaScript: the only `<script>` element is the JSON-LD record.
#[test]
fn the_docs_pages_hold_no_script() {
    let site = site();
    for (index, page) in site.pages.iter().enumerate() {
        let html = radian_tui::site_docs::page_html(&site, index);
        let scripts = html.to_lowercase().matches("<script").count();
        assert_eq!(scripts, 1, "{} has one script element", page.name);
        assert!(
            html.contains(JSON_LD_OPEN),
            "{} has only the JSON-LD element",
            page.name
        );
        assert!(
            !html.contains("onclick"),
            "{} has no inline handler",
            page.name
        );
    }
}

/// The opening tag of the one `<script>` element a docs page holds.
const JSON_LD_OPEN: &str = "<script type=\"application/ld+json\">";

/// The JSON-LD element's content.
fn json_ld(html: &str) -> serde_json::Value {
    let start = html.find(JSON_LD_OPEN).expect("the JSON-LD element") + JSON_LD_OPEN.len();
    let end = html[start..].find("</script>").expect("the element closes") + start;
    serde_json::from_str(&html[start..end]).expect("the JSON-LD record parses")
}

/// Every docs page carries a `TechArticle` record with the page's title, description and
/// absolute URL, and the record cannot close its element early.
#[test]
fn every_docs_page_carries_a_tech_article_record() {
    let site = site();
    for (index, page) in site.pages.iter().enumerate() {
        let html = radian_tui::site_docs::page_html(&site, index);
        let record = json_ld(&html);
        assert_eq!(record["@context"], "https://schema.org", "{}", page.name);
        assert_eq!(record["@type"], "TechArticle", "{}", page.name);
        assert_eq!(record["headline"], page.title.as_str(), "{}", page.name);
        assert_eq!(
            record["description"],
            page.description.as_str(),
            "{}",
            page.name
        );
        assert_eq!(
            record["url"],
            radian_tui::site_docs::page_url(&page.stem).as_str(),
            "{}",
            page.name
        );
        assert_eq!(
            record["about"]["@type"], "SoftwareApplication",
            "{}",
            page.name
        );
    }
    assert_eq!(
        radian_tui::site_docs::page_url("index"),
        "https://getrad.dev/docs/"
    );
    assert_eq!(
        radian_tui::site_docs::page_url("arc/index"),
        "https://getrad.dev/docs/arc/"
    );
    assert_eq!(
        radian_tui::site_docs::page_url("arc/loop"),
        "https://getrad.dev/docs/arc/loop.html"
    );
}

/// A description holding `</script>` cannot end the JSON-LD element early.
#[test]
fn a_description_cannot_close_the_json_ld_element() {
    let pages: &[(&str, &str)] = &[
        ("index.md", "# Home\n\n## Start\n\n- `evil`\n"),
        (
            "evil.md",
            "---\ndescription: A </script><b> description.\n---\n\n# Evil\n",
        ),
    ];
    let site = render_site(pages).expect("renders");
    let index = site.pages.iter().position(|p| p.stem == "evil").unwrap();
    let html = radian_tui::site_docs::page_html(&site, index);
    assert_eq!(html.matches("</script>").count(), 1, "one closing tag");
    assert_eq!(json_ld(&html)["description"], "A </script><b> description.");
}

/// `llms.txt` lists every manual page exactly once, by its absolute URL, after the two
/// hand-written pages; `llms-full.txt` holds every page's Markdown under its URL, with the
/// `# ` title (not the frontmatter) right after the URL line.
#[test]
fn the_llms_files_list_every_page_once() {
    let site = site();
    let index = radian_tui::site_docs::llms_txt(&site);
    let full = radian_tui::site_docs::llms_full_txt(&site);
    assert!(
        index.starts_with("# radian\n"),
        "llms.txt starts with the title"
    );
    assert!(
        index.contains("(https://getrad.dev/)"),
        "the home page is listed"
    );
    assert!(
        index.contains("(https://getrad.dev/arc.html)"),
        "the arc page is listed"
    );
    assert!(
        index.contains("(https://getrad.dev/llms-full.txt)"),
        "the full file is listed"
    );
    for page in &site.pages {
        let url = radian_tui::site_docs::page_url(&page.stem);
        let line = format!("- [{}]({url})", page.title);
        assert_eq!(
            index.matches(&line).count(),
            1,
            "{} is listed once",
            page.name
        );
        assert!(
            index.contains(&format!("{line}: {}", page.description)),
            "{} is listed with its description",
            page.name
        );
        let header = format!("\nURL: {url}\n\n# ");
        assert_eq!(
            full.matches(&header).count(),
            1,
            "{} is in the full file once",
            page.name
        );
    }
    let urls: Vec<&str> = index
        .lines()
        .filter_map(|l| l.split_once("](https://getrad.dev/docs/").map(|(_, r)| r))
        .collect();
    assert_eq!(urls.len(), site.pages.len(), "exactly the manual pages");
}

/// The sidebar groups come from `index.md` and cover every page exactly once.
#[test]
fn the_sidebar_groups_cover_every_page_once() {
    let site = site();
    let titles: Vec<&str> = site.groups.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(titles, ["Start", "Use", "Configure", "Extend", "Reference"]);
    let mut seen: HashSet<&str> = HashSet::new();
    for group in &site.groups {
        for stem in &group.stems {
            assert!(seen.insert(stem.as_str()), "{stem} is grouped twice");
            assert!(
                site.pages.iter().any(|p| &p.stem == stem),
                "{stem} is a manual page"
            );
        }
    }
    for page in &site.pages {
        if page.stem == "index" {
            continue;
        }
        assert!(
            seen.contains(page.stem.as_str()),
            "{} is in a sidebar group",
            page.name
        );
    }
}

/// Mutation: a link to a page that is not in the table fails the build, naming the
/// source page and the link.
#[test]
fn a_link_to_a_missing_page_fails_the_build() {
    let pages = [
        ("a.md", "# A\n\nSee [gone](missing.md).\n"),
        ("b.md", "# B\n\n## Here\n"),
    ];
    let error = render_site(&pages).expect_err("a link to missing.md must fail");
    assert_eq!(error.page, "a.md");
    assert_eq!(error.link, "missing.md");
    assert!(
        error.to_string().contains("a.md") && error.to_string().contains("missing.md"),
        "the failure names the source and the link: {error}"
    );
}

/// An arc page renders at `arc/<page>.html` and reaches the site root's assets from its own
/// directory.
#[test]
fn an_arc_page_renders_in_its_subdirectory() {
    let site = site();
    let index = site
        .pages
        .iter()
        .position(|page| page.stem == "arc/gate")
        .expect("arc/gate renders");
    assert_eq!(
        radian_tui::site_docs::output_path(&site.pages[index].stem),
        "arc/gate.html"
    );
    let html = radian_tui::site_docs::page_html(&site, index);
    assert!(
        html.contains("<link rel=\"stylesheet\" href=\"../../docs.css\">"),
        "an arc page's stylesheet link resolves from arc/: {html}"
    );
    assert!(
        html.contains("href=\"../../favicon.svg\""),
        "an arc page's icon link resolves from arc/: {html}"
    );
    // The root pages still reach the same assets one level up.
    let root = site
        .pages
        .iter()
        .position(|page| page.stem == "index")
        .expect("index renders");
    let root_html = radian_tui::site_docs::page_html(&site, root);
    assert!(
        root_html.contains("<link rel=\"stylesheet\" href=\"../docs.css\">"),
        "a root page's asset link: {root_html}"
    );
}

/// Page links are written relative to their source page, as GitHub renders them, and resolve
/// in both directions across the `arc/` boundary.
#[test]
fn relative_page_links_resolve_from_the_source_directory() {
    let pages = [
        (
            "arc/loop.md",
            "# Loop\n\n## Deep\n\nSee [gate](gate.md#here), [config](../configuration.md#setup), and [home](../index.md).\n",
        ),
        ("arc/gate.md", "# Gate\n\n## Here\n"),
        (
            "configuration.md",
            "# Config\n\n## Setup\n\nSee [the loop](arc/loop.md#deep).\n",
        ),
        ("index.md", "# Home\n"),
    ];
    let site = render_site(&pages).expect("relative links resolve");
    let loop_page = site
        .pages
        .iter()
        .find(|page| page.stem == "arc/loop")
        .expect("the loop page renders");
    assert!(
        loop_page.body.contains("href=\"gate.html#here\""),
        "a sibling link: {}",
        loop_page.body
    );
    assert!(
        loop_page
            .body
            .contains("href=\"../configuration.html#setup\""),
        "a parent link: {}",
        loop_page.body
    );
    assert!(
        loop_page.body.contains("href=\"../index.html\""),
        "a link to the landing page: {}",
        loop_page.body
    );
    let config = site
        .pages
        .iter()
        .find(|page| page.stem == "configuration")
        .expect("the configuration page renders");
    assert!(
        config.body.contains("href=\"arc/loop.html#deep\""),
        "a link into arc/: {}",
        config.body
    );
}

/// A link from an arc page to a missing sibling fails the build, not silently becomes a repo
/// link.
#[test]
fn a_link_to_a_missing_sibling_from_an_arc_page_fails_the_build() {
    let pages = [("arc/loop.md", "# Loop\n\nSee [gone](missing.md).\n")];
    let error = render_site(&pages).expect_err("a missing sibling must fail");
    assert_eq!(error.page, "arc/loop.md");
    assert_eq!(error.link, "missing.md");
}

/// Mutation: a fragment without a heading fails the build too.
#[test]
fn a_link_to_a_missing_heading_fails_the_build() {
    let pages = [("a.md", "# A\n\n## Here\n\nSee [below](#gone).\n")];
    let error = render_site(&pages).expect_err("a link to #gone must fail");
    assert_eq!(error.link, "#gone");
}

/// Every `/docs/…` link in the hand-written site pages names a page the generator writes and,
/// with a fragment, a heading on that page.
#[test]
fn the_hand_written_pages_only_link_generated_docs() {
    let site = site();
    let by_output: std::collections::HashMap<String, &radian_tui::site_docs::RenderedPage> = site
        .pages
        .iter()
        .map(|page| (radian_tui::site_docs::output_path(&page.stem), page))
        .collect();
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (file, _) in HAND_WRITTEN_PAGES {
        let source = root.join("site").join(file);
        let html = std::fs::read_to_string(&source)
            .unwrap_or_else(|error| panic!("{}: {error}", source.display()));
        for href in hrefs(&html) {
            let Some(rest) = href.strip_prefix("/docs/") else {
                continue;
            };
            let (target, fragment) = match rest.split_once('#') {
                Some((target, fragment)) => (target, Some(fragment)),
                None => (rest, None),
            };
            // The generator writes `docs/index.html`; `/docs/` and `/docs/arc/` are the
            // directory indexes GitHub Pages serves from those files.
            let output = if target.is_empty() {
                "index.html".to_owned()
            } else if target.ends_with('/') {
                format!("{target}index.html")
            } else {
                target.to_owned()
            };
            let page = by_output.get(&output).unwrap_or_else(|| {
                panic!(
                    "site/{file}: link `{href}` names `{output}`, which the generator does not write"
                )
            });
            if let Some(id) = fragment
                && !id.is_empty()
                && !page.headings.iter().any(|heading| heading.id == id)
            {
                panic!("site/{file}: link `{href}` has no heading `#{id}` on `{output}`");
            }
        }
    }
}

/// Resolves an output-relative href from `page_stem` to a docs-root-relative path.
fn resolve_href(page_stem: &str, path: &str) -> Option<String> {
    let mut parts: Vec<&str> = page_stem
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("")
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
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

/// Every `href="…"` in a fragment of HTML.
fn hrefs(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("href=\"") {
        rest = &rest[start + 6..];
        if let Some(end) = rest.find('"') {
            out.push(&rest[..end]);
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// Every docs page names its absolute URL once in a `canonical` link.
#[test]
fn every_docs_page_names_its_canonical_url() {
    let site = site();
    for (index, page) in site.pages.iter().enumerate() {
        let html = radian_tui::site_docs::page_html(&site, index);
        let link = format!(
            "<link rel=\"canonical\" href=\"{}\">",
            radian_tui::site_docs::page_url(&page.stem)
        );
        assert_eq!(
            html.matches(&link).count(),
            1,
            "{} has its canonical",
            page.name
        );
        assert_eq!(
            html.matches("rel=\"canonical\"").count(),
            1,
            "{}",
            page.name
        );
    }
}

/// The hand-written pages name their canonical URL; the 404 page is `noindex` and names
/// none. The home page's share image is a file the site serves.
#[test]
fn the_hand_written_pages_name_their_canonical_url() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (file, canonical) in HAND_WRITTEN_PAGES {
        let html = std::fs::read_to_string(root.join("site").join(file)).unwrap();
        match canonical {
            Some(url) => {
                let link = format!("<link rel=\"canonical\" href=\"{url}\">");
                assert_eq!(
                    html.matches(&link).count(),
                    1,
                    "site/{file} has its canonical"
                );
                assert!(!html.contains("noindex"), "site/{file} is indexable");
            }
            None => {
                assert!(
                    !html.contains("rel=\"canonical\""),
                    "site/{file} has no canonical"
                );
                assert!(
                    html.contains("<meta name=\"robots\" content=\"noindex\">"),
                    "site/{file} is noindex"
                );
            }
        }
    }
    let home = std::fs::read_to_string(root.join("site/index.html")).unwrap();
    let image = home
        .split("<meta property=\"og:image\" content=\"https://getrad.dev/")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the home page names an og:image under the site");
    assert!(
        root.join("site").join(image).is_file(),
        "site/{image} is a file the site serves"
    );
}

/// `sitemap.xml` lists the hand-written pages and every manual page exactly once.
#[test]
fn the_sitemap_lists_every_page_once() {
    let site = site();
    let xml = radian_tui::site_docs::sitemap_xml(&site);
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset "));
    assert!(xml.ends_with("</urlset>\n"));
    let locs: Vec<&str> = xml
        .lines()
        .filter_map(|l| l.strip_prefix("<url><loc>"))
        .map(|l| l.trim_end_matches("</loc></url>"))
        .collect();
    let mut expected = vec![
        "https://getrad.dev/".to_owned(),
        "https://getrad.dev/arc.html".to_owned(),
    ];
    expected.extend(
        site.pages
            .iter()
            .map(|p| radian_tui::site_docs::page_url(&p.stem)),
    );
    assert_eq!(
        locs, expected,
        "the sitemap is exactly the site's pages, in order"
    );
    let unique: HashSet<&str> = locs.iter().copied().collect();
    assert_eq!(unique.len(), locs.len(), "no URL twice");
}

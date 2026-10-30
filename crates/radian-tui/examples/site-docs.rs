//! Generates the static docs site from the `DOCS` table the binary embeds.
//!
//! Run: `cargo run -p radian-tui --example site-docs --locked -- <out-dir>`.
//! Writes `<out-dir>/docs/index.html` (from `index.md`) and
//! `<out-dir>/docs/<stem>.html` for every other page, in the page's own subdirectory
//! (`docs/arc/loop.html`), then `<out-dir>/llms.txt` (the page index),
//! `<out-dir>/llms-full.txt` (every page's Markdown) and `<out-dir>/sitemap.xml`. A broken
//! inter-page link fails the build with the source page
//! and the link; a page holding raw HTML is reported on stderr (the HTML is escaped in the
//! output). It also splits the two bilingual source pages `site/index.html` and
//! `site/arc.html` into `index.html`, `arc.html`, `zh/index.html` and `zh/arc.html`.

use std::path::PathBuf;

fn main() {
    let out: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("usage: site-docs <out-dir>");
            std::process::exit(2);
        });
    let site = match radian_tui::site_docs::render_site(radian_coding::docs::DOCS) {
        Ok(site) => site,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let docs = out.join("docs");
    if let Err(error) = std::fs::create_dir_all(&docs) {
        eprintln!("site-docs: cannot create {}: {error}", docs.display());
        std::process::exit(1);
    }
    for (index, page) in site.pages.iter().enumerate() {
        let file = radian_tui::site_docs::output_path(&page.stem);
        let target = docs.join(&file);
        if let Some(parent) = target.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("site-docs: cannot create {}: {error}", parent.display());
            std::process::exit(1);
        }
        let html = radian_tui::site_docs::page_html(&site, index);
        if let Err(error) = std::fs::write(&target, html) {
            eprintln!("site-docs: cannot write {file}: {error}");
            std::process::exit(1);
        }
        if page.raw_html {
            eprintln!("site-docs: {} contains raw HTML (escaped)", page.name);
        }
    }
    let site_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site");
    for page in radian_tui::site_docs::HAND_WRITTEN_PAGES {
        let source = site_dir.join(page.file);
        let text = std::fs::read_to_string(&source)
            .map_err(|error| format!("site-docs: cannot read {}: {error}", source.display()))
            .and_then(|text| {
                radian_tui::site_docs::split_page(&text, page.file, page.lang)
                    .map_err(|error| error.to_string())
            });
        let text = match text {
            Ok(text) => text,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        let target = out.join(page.path.trim_start_matches('/'));
        let target = if page.path.ends_with('/') {
            target.join("index.html")
        } else {
            target
        };
        if let Some(parent) = target.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("site-docs: cannot create {}: {error}", parent.display());
            std::process::exit(1);
        }
        if let Err(error) = std::fs::write(&target, text) {
            eprintln!("site-docs: cannot write {}: {error}", target.display());
            std::process::exit(1);
        }
    }
    for (file, text) in [
        ("llms.txt", radian_tui::site_docs::llms_txt(&site)),
        ("llms-full.txt", radian_tui::site_docs::llms_full_txt(&site)),
        ("sitemap.xml", radian_tui::site_docs::sitemap_xml(&site)),
    ] {
        if let Err(error) = std::fs::write(out.join(file), text) {
            eprintln!("site-docs: cannot write {file}: {error}");
            std::process::exit(1);
        }
    }
}

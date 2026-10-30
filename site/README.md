# getrad.dev

Static site: two pages, `index.html` (radian) and `arc.html` (arc), each in one file with English and Chinese in the same page, switched by the `EN / 中` button, plus `404.html` (GitHub Pages serves it for any missing path; `noindex`, docs chrome) and `robots.txt` (allows everything, names the sitemap). `media/` holds the arc videos and their poster frames, and `og-radian.png`, the 1200×630 share card the home page names as `og:image` (the README banner `.github/readme/banner-light.png` on the paper colour; remake it with the same banner when the wordmark changes). No build step, no JavaScript dependencies. Fonts come from Google Fonts (Geist, JetBrains Mono, and Sora for the six letters of the wordmark).

Preview: `python3 -m http.server -d site 8000`.

## Machine-readable pages

Each hand-written page carries one Schema.org `SoftwareApplication` record in a
`<script type="application/ld+json">` element in its `<head>`; every generated docs page
carries a `TechArticle` record with its title, description and absolute URL. The generator
also writes `llms.txt` (one line per page with its description, in sidebar order, after the
two hand-written pages), `llms-full.txt` (every manual page's Markdown under its URL) and
`sitemap.xml` (every page's URL) at the site root, and every docs page names its absolute
URL in a `canonical` link; the two hand-written pages name theirs by hand. All of it comes from the `DOCS` table, so a new manual page is listed
without any edit here; the two hand-written pages' lines live in
`crates/radian-tui/src/site_docs.rs` (`HAND_WRITTEN_PAGES`). GitHub Pages sends no custom
response headers, so the site has no `Strict-Transport-Security` header; it redirects
`http://` to `https://` with a 301.

## The terminal figure

Figure 1 is a frame rendered by the TUI prototype script, pasted into `index.html` twice (`.frame-light`, `.frame-dark`). To regenerate:

```sh
for t in light dark; do
  bash research/2026-10-16-render-prototype/render-screen.sh --width 120 --panel off --theme $t > /tmp/screen-$t.ansi
done
python3 -I site/tools/ansi2html.py /tmp/screen-light.ansi 116,89,180   # paste into <pre class="frame-light">
python3 -I site/tools/ansi2html.py /tmp/screen-dark.ansi 167,152,215   # paste into <pre class="frame-dark">
```

The two numbers are the theme's accent RGB; the script maps it to the page's `var(--accent)`. Box-drawing characters are drawn by CSS (`.bx`), not by the font, because the Google Fonts subset of JetBrains Mono has none and a fallback font breaks the lines.

## Before launch

- The old speed numbers were removed; they come back only after a re-measure (`benchmarks/perf-baseline.md`).
- The frame's model name, costs and quota are invented by the prototype script.

## Deploy

`getrad.dev` is hosted on GitHub Pages by `.github/workflows/pages.yml`.
Every push to `main` that changes `site/**`, `docs/user/**`, the generator
(`crates/radian-tui/examples/site-docs.rs`, `crates/radian-tui/src/site_docs.rs`)
or the workflow itself deploys; `workflow_dispatch` deploys on demand. The job
copies `index.html`, `arc.html`, `404.html`, `robots.txt`, `favicon.svg`, `docs.css` and `media/` from `site/` into `_site/`,
runs the generator into `_site/docs/` (which also writes `_site/llms.txt`,
`_site/llms-full.txt` and `_site/sitemap.xml`), writes `_site/CNAME` with `getrad.dev`,
then configures and uploads the Pages artifact.

## Preview the docs

```sh
cargo run -p radian-tui --example site-docs --locked -- /tmp/site
cp site/index.html site/arc.html site/favicon.svg site/docs.css /tmp/site/
cp -r site/media /tmp/site/media
python3 -m http.server -d /tmp/site 8000
```

then open `http://localhost:8000/docs/`, `http://localhost:8000/llms.txt`, `http://localhost:8000/llms-full.txt` and `http://localhost:8000/sitemap.xml`.

## One source

The docs pages are generated from the same `DOCS` table the binary embeds
(`crates/radian-coding/src/docs.rs`), never from a second copy: the generator
reads the table through the `radian-tui` library. A broken link between pages
fails the build, so it fails CI before merge.

## README images

`.github/readme/` holds the README banner and terminal figure, light and dark, rendered from this page by headless Chrome: `python3 -I site/tools/readme-images.py`. The terminal figure drops the rows between the diff and the job notices (the ask box, the summary, the table, the compaction line) and the theme line under the footer. The model name, costs and quota in the footer are invented by the prototype script.

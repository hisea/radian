"""Put the arc workflow diagram into site/arc.html, in the site's own colours.

The diagram is drawn by archify (an HTML viewer with its own themes). This script takes the SVG out of the two
viewer files (English, Chinese), drops the viewer's styles, scripts and data-* attributes, gives its ids a
language suffix, and pastes the result into site/arc.html between the `<!-- flow-en -->` / `<!-- flow-zh -->`
markers. The colours come from the `/* flow */` block in arc.html, which maps the diagram's classes to the
page's tokens, so the diagram follows the page's light and dark themes.

Usage: python3 -I site/tools/archify-flow.py EN.html ZH.html [--png]
  --png   also render .github/readme/arc-flow-{en,zh}-{light,dark}.png (needs Chrome)
"""
import re, subprocess, sys, tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PAGE = ROOT / "site/arc.html"
OUT = ROOT / ".github/readme"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


def clean(html: str, lang: str) -> str:
    i = html.index("<svg viewBox")
    svg = html[i:html.index("</svg>", i) + 6]
    title = re.search(r'<title id="archify-diagram-title">(.*?)</title>', svg).group(1)
    svg = re.sub(r"<style>.*?</style>", "", svg, flags=re.S)
    svg = re.sub(r"<!--.*?-->", "", svg, flags=re.S)
    svg = re.sub(r"<title\b.*?</title>|<desc\b.*?</desc>", "", svg, flags=re.S)
    svg = re.sub(r' (?:data-[a-z-]+|tabindex|role|aria-pressed|aria-label|aria-labelledby|lang)="[^"]*"', "", svg)
    svg = re.sub(r' data-[a-z-]+(?= |>|/)', "", svg)
    svg = re.sub(r' style="--step:\d+"', "", svg)
    svg = svg.replace('aria-hidden="true" ', "")
    svg = re.sub(r'(id="|url\(#)(arrowhead[a-z-]*|grid)', lambda m: f"{m.group(1)}{m.group(2)}-{lang}", svg)
    svg = svg.replace("<svg viewBox", f'<svg class="flow" role="img" aria-label="{title}" viewBox', 1)
    svg = re.sub(r"\n\s*\n+", "\n", svg)
    return svg


def paste(page: str, lang: str, svg: str) -> str:
    a, b = f"<!-- flow-{lang} -->", f"<!-- /flow-{lang} -->"
    i, j = page.index(a) + len(a), page.index(b)
    return page[:i] + "\n" + svg + "\n" + page[j:]


def render(page: str, lang: str, svg: str, theme: str) -> None:
    tokens = re.search(r":root \{.*?(?=/\* language)", re.search(r"<style>(.*?)</style>", page, re.S).group(1), re.S).group(0)
    flow = re.search(r"/\* flow \*/.*?/\* /flow \*/", page, re.S).group(0)
    doc = f'<!doctype html><html data-theme="{theme}"><meta charset="utf-8"><style>{tokens}{flow}body{{margin:0;background:var(--sheet)}}.flow{{min-width:0}}</style><body>{svg}'
    w, h = map(float, re.search(r'viewBox="0 0 ([\d.]+) ([\d.]+)"', svg).groups())
    h = round(1040 * h / w)
    with tempfile.TemporaryDirectory() as tmp:
        f = Path(tmp) / "f.html"
        f.write_text(doc)
        png = OUT / f"arc-flow-{lang}-{theme}.png"
        png.unlink(missing_ok=True)
        subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", f"--user-data-dir={tmp}/p",
                        "--force-device-scale-factor=2", f"--window-size=1040,{h}", f"--screenshot={OUT}/arc-flow-{lang}-{theme}.png",
                        f"file://{f}"], capture_output=True)
        if not png.exists():  # Chrome can exit non-zero after it has written the screenshot, so look for the file
            sys.exit(f"Chrome wrote no {png.name}")


if __name__ == "__main__":
    en, zh = Path(sys.argv[1]).read_text(), Path(sys.argv[2]).read_text()
    page = PAGE.read_text()
    svgs = {"en": clean(en, "en"), "zh": clean(zh, "zh")}
    for lang, svg in svgs.items():
        page = paste(page, lang, svg)
    PAGE.write_text(page)
    if "--png" in sys.argv:
        for lang, svg in svgs.items():
            for theme in ("light", "dark"):
                render(page, lang, svg, theme)

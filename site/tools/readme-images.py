"""Render the README banner and terminal figure from site/index.html (light and dark).

Usage: python3 -I site/tools/readme-images.py [OUT_DIR]   (default .github/readme; needs Chrome and Pillow)
"""
import re, subprocess, sys, tempfile
from pathlib import Path
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SITE = ROOT / "site/index.html"
OUT = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / ".github/readme"
TMP = Path(tempfile.mkdtemp())
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

src = SITE.read_text()
style = re.search(r"<style>(.*?)</style>", src, re.S).group(1)
fonts = "\n".join(re.findall(r'<link rel="stylesheet"[^>]*>', src))
mark = re.search(r'<svg class="mark-a".*?</svg>', src, re.S).group(0)


def accent(thin: float, thick: float) -> str:
    """The accent of the mark as a filled annular sector: its outer edge is flush with the outline's
    outer edge (18 + thin/2), it is `thick` wide, and both ends are cut flat along the outer edge of
    the radius they meet (the base line and the radius at one radian): the accent covers the end of
    that radius and nothing crosses the outline.
    The page draws the outline 1.5 wide and the accent 2.6; the banner draws them 1.1 and 2.2."""
    import math
    cx, cy, r = 3, 21, 18
    ro, ri, off = r + thin / 2, r + thin / 2 - thick, -thin / 2
    pt = lambda rad, t: (cx + rad * math.cos(t), cy - rad * math.sin(t))
    a = lambda rad: math.asin(off / rad)
    b = lambda rad: 1 - math.asin(off / rad)
    f = lambda p: f"{p[0]:.2f} {p[1]:.2f}"
    return (f"M{f(pt(ri, a(ri)))} L{f(pt(ro, a(ro)))} A{ro:g} {ro:g} 0 0 0 {f(pt(ro, b(ro)))} "
            f"L{f(pt(ri, b(ri)))} A{ri:g} {ri:g} 0 0 1 {f(pt(ri, a(ri)))} Z")


assert accent(1.5, 2.6) in mark
mark = mark.replace(accent(1.5, 2.6), accent(1.1, 2.2))
figure = re.search(r'(<div class="term">.*?)\s*<figcaption', src, re.S).group(1)


def rows(m: re.Match) -> str:
    # keep the turn down to the end of the diff, then the notices, the input and the footer
    lines = m.group(2).split("\n")
    text = [re.sub(r"<[^>]*>", "", l) for l in lines]
    diff_end = next(i for i, t in enumerate(text) if t.startswith("│  127"))
    notices = next(i for i, t in enumerate(text) if "background job bg-2" in t)
    theme = next(i for i, t in enumerate(text) if t.startswith("theme:"))
    return m.group(1) + "\n".join(lines[: diff_end + 2] + lines[notices : theme - 1]) + m.group(3)


figure = re.sub(r'(<pre class="frame-\w+">)(.*?)(</pre>)', rows, figure, flags=re.S)
# the rows above no longer fill 74 lines, so the bar shows only the width
figure = figure.replace("<span>120 × 74</span>", "<span>120</span>")

BANNER = """<!doctype html><html data-theme="{theme}"><head><meta charset="utf-8">{fonts}
<style>{style}
html, body {{ background: transparent !important; }}
body {{ margin: 0; width: 960px; height: 260px; display: flex; align-items: center; justify-content: center; }}
.b {{ display: flex; align-items: center; gap: 36px; color: var(--ink); }}
.b .mark-a {{ width: 150px; height: auto; }}
.b .mark-a path:first-child {{ stroke-width: 1.1; }}
.word {{ font: 600 124px/1 "Sora", sans-serif; letter-spacing: -0.045em; margin: 0 0 14px -4px; }}
.rule {{ height: 2px; background: var(--accent); width: 520px; }}
.sub {{ font: 600 19px/1 "JetBrains Mono", monospace; letter-spacing: 0.14em; color: var(--accent); margin-top: 16px; text-transform: uppercase; }}
</style></head><body><div class="b">{mark}<div><div class="word">radian</div><div class="rule"></div>
<div class="sub">coding agent · rust · one binary</div></div></div></body></html>"""

FRAME = """<!doctype html><html data-theme="{theme}"><head><meta charset="utf-8">{fonts}
<style>{style}
html, body {{ background: transparent !important; }}
body {{ margin: 0; padding: 0; width: max-content; }}
.term-scroll {{ max-height: none; overflow: visible; }}
</style></head><body>{figure}</body></html>"""


def shot(html: str, name: str, size: str) -> None:
    page = TMP / f"{name}.html"
    page.write_text(html)
    png = OUT / f"{name}.png"
    subprocess.run([CHROME, "--headless=new", f"--user-data-dir={TMP / 'profile'}", "--disable-gpu", "--hide-scrollbars",
                    "--force-device-scale-factor=2", f"--window-size={size}",
                    "--default-background-color=00000000", "--virtual-time-budget=6000",
                    f"--screenshot={png}", page.as_uri()],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
    img = Image.open(png)
    img.crop(img.getchannel("A").getbbox()).save(png, optimize=True)


for theme in ("light", "dark"):
    shot(BANNER.format(theme=theme, fonts=fonts, style=style, mark=mark), f"banner-{theme}", "960,260")
    shot(FRAME.format(theme=theme, fonts=fonts, style=style, figure=figure), f"terminal-{theme}", "1100,1400")

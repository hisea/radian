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
.b .mark-a path:last-child {{ stroke-width: 2.2; }}
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

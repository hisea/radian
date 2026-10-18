"""Convert the render-screen.sh truecolor output to an HTML <pre> body.
The theme's accent is mapped to var(--accent) so the page and the frame share one violet."""
import html, re, sys

src, accent = sys.argv[1], tuple(int(x) for x in sys.argv[2].split(","))
text = open(src, encoding="utf-8").read()
lines = text.split("\n")
# drop the trailing "theme: ..." label and blank tail
while lines and (not lines[-1].strip() or lines[-1].startswith("theme:")):
    lines.pop()
text = "\n".join(lines)

out, fg, bold, ital, open_ = [], None, False, False, False
pos = 0
def close():
    global open_
    if open_:
        out.append("</span>"); open_ = False
def start():
    global open_
    styles = []
    if fg is not None:
        styles.append("color:var(--accent)" if fg == accent else "color:rgb(%d,%d,%d)" % fg)
    if bold: styles.append("font-weight:600")
    if ital: styles.append("font-style:italic")
    if styles:
        out.append('<span style="%s">' % ";".join(styles)); open_ = True
# Box drawing is drawn by CSS (.bx), not by the font: web font subsets lack these glyphs
# and a fallback font's metrics break the lines. Classes name the half-segments drawn.
BOX = {"│": "u d", "─": "l r", "━": "l r heavy", "┌": "d r", "┐": "d l", "└": "u r", "┘": "u l",
       "├": "u d r", "┤": "u d l", "┬": "l r d", "┴": "l r u", "┼": "u d l r",
       "╭": "arc-dr", "╰": "arc-ur", "▏": "bar"}
def cells(seg):
    # Pin every non-ASCII glyph outside box drawing to one cell, so a fallback font's
    # different advance cannot push the rest of the row sideways.
    res = []
    for ch in seg:
        if ch in BOX:
            res.append('<span class="bx %s">%s</span>' % (BOX[ch], html.escape(ch)))
        elif ord(ch) > 127:
            res.append('<span class="w">%s</span>' % html.escape(ch))
        else:
            res.append(html.escape(ch))
    return "".join(res)
for m in re.finditer(r"\x1b\[([0-9;]*)m", text):
    out.append(cells(text[pos:m.start()]))
    pos = m.end()
    codes = [int(c) for c in m.group(1).split(";") if c] or [0]
    i = 0
    while i < len(codes):
        c = codes[i]
        if c == 0: fg, bold, ital = None, False, False
        elif c == 1: bold = True
        elif c == 3: ital = True
        elif c == 38 and codes[i + 1] == 2:
            fg = tuple(codes[i + 2:i + 5]); i += 4
        i += 1
    close(); start()
out.append(cells(text[pos:])); close()
sys.stdout.write("".join(out))

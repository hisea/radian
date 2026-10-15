#!/usr/bin/env python3
"""Check the unicode-tier glyphs against the six mainstream coding fonts (ADR 0027
revision 6: Operator Mono left the reference set) and print the coverage table that lives
in `crates/radian-tui/README.md`.

Why: a glyph the font does not have is drawn by a system fallback font with different
metrics, so it sits off the baseline. The `unicode` footer tier must only use characters
that every one of these fonts has.

Usage:
    python3 -m venv .scratch/venv
    .scratch/venv/bin/pip install fonttools
    .scratch/venv/bin/python crates/radian-tui/scripts/check-glyph-coverage.py

fontTools is not a repository dependency: this script is a one-off check, not part of
`cargo test`. A font that is not installed here is printed as `未查` (not checked); the
table never guesses. `JetBrains Mono` is checked through its Nerd Font build
when the plain build is not installed (the patch is additive, so a glyph missing from the
Nerd build is missing upstream too). Keep the GLYPHS list in step with
the coverage test in `crates/radian-tui/src/footer.rs`.
"""

import os
import sys

try:
    from fontTools.ttLib import TTFont, TTCollection
except ImportError:
    sys.exit("fonttools is missing: python3 -m venv .scratch/venv && "
             ".scratch/venv/bin/pip install fonttools")

HOME = os.path.expanduser("~")

# name -> candidate paths, in order. The first that exists is used; `None` means 未查.
FONTS = [
    ("Menlo", ["/System/Library/Fonts/Menlo.ttc"]),
    ("SF Mono", [
        "/System/Applications/Utilities/Terminal.app/Contents/Resources/Fonts/SF-Mono-Regular.otf",
        "/System/Library/Fonts/SFNSMono.ttf",
    ]),
    ("JetBrains Mono", [
        f"{HOME}/Library/Fonts/JetBrainsMono-Regular.ttf",
        f"{HOME}/Library/Fonts/JetBrainsMonoNerdFont-Regular.ttf",
    ]),
    ("Cascadia Code", [f"{HOME}/Library/Fonts/CascadiaCode.ttf"]),
    ("Fira Code", [f"{HOME}/Library/Fonts/FiraCode-Regular.ttf"]),
    ("Source Code Pro", ["/Library/Fonts/SourceCodePro_Regular.ttf"]),
]

# Every glyph the `unicode` tier can draw. The ASCII ones (`R @ * # ^ v ~`) are trivially
# covered and are listed only for the table's completeness.
GLYPHS = [
    ("R", "brand (unicode)"),
    ("@", "host"),
    ("»", "speed"),
    ("*", "reasoning"),
    ("#", "session"),
    ("↑", "tokens in"),
    ("↓", "tokens out"),
    ("R", "tokens cache"),
    ("~", "auto-compaction marker"),
    ("│", "footer separator"),
    ("━", "context bar fill"),
    ("─", "context bar empty"),
    ("▖", "spinner frame 1"),
    ("▘", "spinner frame 2"),
    ("▝", "spinner frame 3"),
    ("▗", "spinner frame 4 (quadrant)"),
    ("⏺", "spinner default (pulse, documented exception)"),
    ("◜", "spinner arc (documented exception)"),
    ("◝", "spinner arc (documented exception)"),
    ("◞", "spinner arc (documented exception)"),
    ("◟", "spinner arc (documented exception)"),
    ("⠋", "spinner braille (documented exception)"),
    ("~", "tool status: running"),
    ("✓", "tool status: success"),
    ("×", "tool status: failure"),
    ("›", "reasoning fold marker"),
    ("…", "truncation"),
]


def load_cmap(path):
    if path.endswith(".ttc"):
        return TTCollection(path).fonts[0].getBestCmap()
    return TTFont(path).getBestCmap()


def main():
    cmaps = []
    for name, paths in FONTS:
        path = next((p for p in paths if os.path.exists(p)), None)
        if path is None:
            cmaps.append((name, None))
        else:
            cmaps.append((name, load_cmap(path)))

    header = "| glyph | codepoint | use | " + " | ".join(n for n, _ in cmaps) + " |"
    print(header)
    print("|" + "---|" * (3 + len(cmaps)))
    for glyph, use in GLYPHS:
        cp = ord(glyph)
        cells = []
        for _, cmap in cmaps:
            if cmap is None:
                cells.append("未查")
            else:
                cells.append("✅" if cp in cmap else "❌")
        print(f"| `{glyph}` | U+{cp:04X} | {use} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()

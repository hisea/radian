"""Theme loading for the rendering prototypes.

`load(name)` returns a palette: the prototype's colour names → (r, g, b) or None (the
terminal's default), plus `page_bg` when the theme declares one.

  dark / light   radian's built-in themes (`theme.rs` `mod pi`, okhsl converted with the same
                 algorithm as `theme/color.rs`; see okhsl.py)
  <pi theme>     a pi theme JSON by name from `crates/radian-tui/tests/data/themes/` (the 19
                 bundled fixtures), or a path to any pi theme file. Values are `#hex`, a bare
                 var name (resolved through `vars`), or `""` (the terminal default). The pi
                 role → prototype colour mapping below follows `theme/file.rs`.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import okhsl  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
THEME_DIRS = [
    os.path.normpath(os.path.join(HERE, "..", "..", "..", "crates", "radian-tui", "tests", "data", "themes")),
    os.path.expanduser("~/.radian/themes"),
]

BUILTIN = {
    "dark": dict(
        TEXT="okhsl(234 3% 89%)", MUTED="okhsl(229 6% 67%)", DIM="okhsl(229 8% 56%)",
        VIOLET="okhsl(295 50% 67%)", BLUE="okhsl(232 54% 67%)", STRING="okhsl(52 67% 67%)",
        GREEN="okhsl(159 59% 67%)", RED="okhsl(20 72% 67%)", YELLOW="okhsl(83 88% 67%)",
        THINKING="okhsl(226 7% 65%)", ACCENT_BAR="okhsl(295 53% 64%)", USER_BG="okhsl(233 41% 24%)",
        CARD_BG="okhsl(229 5% 24%)", ADD_BG="okhsl(158 46% 25%)", DEL_BG="okhsl(19 54% 25%)",
    ),
    "light": dict(
        TEXT="okhsl(225 5% 27%)", MUTED="okhsl(229 8% 47%)", DIM="okhsl(229 7% 59%)",
        VIOLET="okhsl(295 60% 46%)", BLUE="okhsl(231 68% 47%)", STRING="okhsl(52 84% 46%)",
        GREEN="okhsl(159 75% 46%)", RED="okhsl(20 91% 47%)", YELLOW="okhsl(83 99% 47%)",
        THINKING="okhsl(234 8% 55%)", ACCENT_BAR="okhsl(295 59% 55%)", USER_BG="okhsl(235 19% 91%)",
        CARD_BG="okhsl(248 3% 91%)", ADD_BG="okhsl(156 21% 91%)", DEL_BG="okhsl(24 23% 91%)",
    ),
}

# prototype colour name → pi role (theme/file.rs mapping, plus the roles this design adds)
PI_ROLE = dict(
    TEXT="text", MUTED="muted", DIM="dim", VIOLET="accent", BLUE="syntaxKeyword", STRING="syntaxString",
    GREEN="success", RED="error", YELLOW="mdCode", THINKING="thinkingText", ACCENT_BAR="borderAccent",
    USER_BG="userMessageBg", CARD_BG="toolPendingBg", ADD_BG="toolSuccessBg", DEL_BG="toolErrorBg",
    DIFF_ADD="toolDiffAdded", DIFF_DEL="toolDiffRemoved", DIFF_CTX="toolDiffContext",
    HEADING="mdHeading", BULLET="mdListBullet", HR="mdHr", COMMENT="syntaxComment", WARNING="warning",
)


def hex_rgb(s):
    s = s.lstrip("#")
    return tuple(int(s[i:i + 2], 16) for i in (0, 2, 4))


def resolve(value, variables, depth=0):
    """A pi colour value → (r, g, b) or None."""
    if value is None or value == "" or depth > 8:
        return None
    value = str(value).strip()
    if value.startswith("#"):
        return hex_rgb(value)
    if value.startswith("okhsl("):
        return okhsl.parse(value)
    if value.startswith("$"):
        value = value[1:]
    if value in variables:
        return resolve(variables[value], variables, depth + 1)
    return None


def find_theme_file(name):
    if os.path.isfile(name):
        return name
    for directory in THEME_DIRS:
        path = os.path.join(directory, name + ".json")
        if os.path.isfile(path):
            return path
    return None


def load(name):
    """Returns (palette, page_bg, label)."""
    if name in BUILTIN:
        pal = {k: okhsl.parse(v) for k, v in BUILTIN[name].items()}
        pal.update(DIFF_ADD=pal["GREEN"], DIFF_DEL=pal["RED"], DIFF_CTX=pal["MUTED"], HEADING=pal["TEXT"],
                   BULLET=pal["VIOLET"], HR=pal["DIM"], COMMENT=pal["DIM"], WARNING=pal["YELLOW"])
        return pal, None, f"built-in {name}"
    path = find_theme_file(name)
    if path is None:
        sys.exit(f"theme `{name}` not found: not built-in, not in {THEME_DIRS}, not a file")
    data = json.load(open(path, encoding="utf-8"))
    variables = data.get("vars") or {}
    colors = data.get("colors") or {}
    pal = {}
    for key, role in PI_ROLE.items():
        pal[key] = resolve(colors.get(role), variables)
    # fallbacks when a pi theme leaves a role unset (ADR 0029 decision 4 falls back to the
    # built-in; the prototype falls back inside the theme so the picture stays one theme)
    fb = {"DIFF_ADD": "GREEN", "DIFF_DEL": "RED", "DIFF_CTX": "MUTED", "HEADING": "TEXT", "BULLET": "VIOLET",
          "HR": "DIM", "COMMENT": "DIM", "WARNING": "YELLOW", "YELLOW": "STRING", "BLUE": "VIOLET",
          "STRING": "TEXT", "THINKING": "MUTED", "ACCENT_BAR": "VIOLET"}
    for key, other in fb.items():
        if pal.get(key) is None:
            pal[key] = pal.get(other)
    page_bg = resolve((data.get("export") or {}).get("pageBg"), variables)
    return pal, page_bg, f"pi theme {data.get('name') or os.path.basename(path)}"


def available():
    names = ["dark", "light"]
    for directory in THEME_DIRS:
        if os.path.isdir(directory):
            for f in sorted(os.listdir(directory)):
                if f.endswith(".json") and f[:-5] not in names:
                    names.append(f[:-5])
    return names

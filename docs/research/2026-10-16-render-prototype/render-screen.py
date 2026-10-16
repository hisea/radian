#!/usr/bin/env python3
"""radian full-screen rendering prototype (2026-10-16): one unified look for every component.

Prints a whole TUI frame with ANSI truecolor in radian's built-in dark palette:

  transcript   user line, thinking (expanded / folded), a shell card, a `read` one-liner,
               a `patch` diff, an `ask_user` block, an assistant Markdown reply, the
               compaction rule, task notices and a warning
  menu         the `/` command menu above the input (--menu on)
  modal        an approval panel or the Ask panel in place of the input (--modal ...)
  input        the status row and the input line (--input thin | bar | lines)
  footer       the three `deck` rows (ADR 0027)
  panel        the orchestration board on the right (--panel on): agents, queue, pending,
               a mini dashboard (ADR 0030 O10's three built-in panels, folded into one)

The rules the proposal keeps everywhere (that is the point: one set, every component):

  * a 2-cell marker column, the body starts at column 3 (ADR 0028 rule 2);
    one glyph per kind: ❯ user · ● reply · ▸/▾ thinking · • tool · * notice · ! warning
  * one line weight: thin `│ ─ ╭ ╮ ╰ ╯` for everything that is a frame (code, diff,
    thinking body, panels, the side panel divider); no background bands in the transcript
  * one accent (VIOLET) for the user line, headings, list markers, the cursor and the rail;
    GREEN / RED only mean success / failure; everything structural is DIM
  * exactly one blank row between blocks (ADR 0035 V1), none inside a block

Switches: --width N (default 110) --panel on|off --menu on|off --modal none|approval|ask
          --input thin|bar|lines --marks on|off --numbers on|off --today (the current look, for
          the transcript only; reconstructed from origin/main 5836485)
          --theme dark|light|<pi theme>  (radian's built-ins, or one of the 19 bundled pi
          themes such as tokyo-night, catppuccin-latte, gruvbox-dark; a pi theme paints its
          own page background)
"""
import argparse
import sys

import importlib.util
import os

_spec = importlib.util.spec_from_file_location(
    "render_prototype", os.path.join(os.path.dirname(os.path.abspath(__file__)), "render-prototype.py"))
_rp = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_rp)
TEXT, MUTED, DIM, VIOLET, BLUE, GREEN, RED, YELLOW, THINKING = (
    _rp.TEXT, _rp.MUTED, _rp.DIM, _rp.VIOLET, _rp.BLUE, _rp.GREEN, _rp.RED, _rp.YELLOW, _rp.THINKING)
ACCENT_BAR, USER_BG, CARD_BG, RESET, BOLD, ITALIC = (
    _rp.ACCENT_BAR, _rp.USER_BG, _rp.CARD_BG, _rp.RESET, _rp.BOLD, _rp.ITALIC)
fg, bg, width, inline, wrap_words, strip_inline, highlight = (
    _rp.fg, _rp.bg, _rp.width, _rp.inline, _rp.wrap_words, _rp.strip_inline, _rp.highlight)
THOUGHT, THOUGHT_SECS, PATCH_PATH, DIFF = _rp.THOUGHT, _rp.THOUGHT_SECS, _rp.PATCH_PATH, _rp.DIFF
PAGE_BG = None


def apply_theme(name):
    """Loads a theme into render-prototype's palette and re-binds this module's names."""
    import os
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import themes
    palette, page_bg, label = themes.load(name)
    _rp.set_palette(palette)
    g = globals()
    for n in _rp.PALETTE_NAMES:
        g[n] = getattr(_rp, n)
    g["PAGE_BG"] = page_bg
    return label


class Opt:
    """The subset of switches the shared helpers read."""
    bold = "plain"
    code = "text"
    header = "bold"


# ---- a row buffer: rows of spans, rendered to an exact width ------------------------------
class Buf:
    def __init__(self, cols):
        self.cols = cols
        self.rows = []          # each row: (spans, row_bg)

    def line(self, spans, row_bg=None):
        self.rows.append((list(spans), row_bg))

    def blank(self):
        self.rows.append(([], None))

    def halfpad(self, top, color, rail=None):
        glyph = "▄" if top else "▀"
        row = glyph * self.cols
        if rail:
            self.rows.append(([("▖" if top else "▘", rail, "", None), (row[1:], color, "", None)], None))
        else:
            self.rows.append(([(row, color, "", None)], None))


def render_row(spans, row_bg, cols):
    buf, used = [], 0
    for text, color, attrs, span_bg in spans:
        b = span_bg if span_bg else row_bg
        while text and used + width(text) > cols:
            text = text[:-1]
        if not text:
            continue
        buf.append(f"{RESET}{bg(b)}{fg(color)}{attrs}{text}")
        used += width(text)
    buf.append(f"{RESET}{bg(row_bg)}{' ' * max(0, cols - used)}{RESET}")
    return "".join(buf)


def render_row_bg(spans, row_bg, cols):
    """`render_row` with the theme's page background under rows that have none."""
    return render_row(spans, row_bg or PAGE_BG, cols)


M = [("  ", TEXT, "", None)]      # the empty marker column
RAIL = [("│ ", DIM, "", None)]    # the thin rail under a marker


def mark(glyph, color=DIM):
    return [(glyph + " ", color, "", None)]


# ---- transcript blocks ---------------------------------------------------------------------
def user_line(b, opt, text):
    if opt.today:
        b.halfpad(True, USER_BG, rail=ACCENT_BAR)
        b.line([("▌ ", ACCENT_BAR, "", None), (text, TEXT, "", None)], row_bg=USER_BG)
        b.halfpad(False, USER_BG, rail=ACCENT_BAR)
        return
    b.line(mark("❯", VIOLET) + [(text, VIOLET, "", None)])


def thinking(b, opt, folded):
    body = b.cols - 2
    if opt.today:
        if folded:
            b.line(mark("▸", THINKING) + [("thinking (8 lines)", THINKING, ITALIC, None)])
            return
        first = True
        for para in THOUGHT:
            for row in (wrap_words(para, body) if para else [""]):
                b.line([("~ " if first else "  ", THINKING, "", None), (row, THINKING, ITALIC, None)])
                first = False
        return
    if folded:
        b.line(mark("▸", THINKING) + [(f"thinking · {THOUGHT_SECS} · 8 lines", THINKING, "", None)])
        return
    b.line(mark("▾", THINKING) + [(f"thinking · {THOUGHT_SECS}", THINKING, "", None)])
    for para in THOUGHT:
        if para == "":
            b.line(RAIL)
        elif para.startswith("- "):
            for k, row in enumerate(wrap_words(para[2:], body - 4)):
                b.line(RAIL + [("- " if k == 0 else "  ", THINKING, "", None)]
                       + inline(row, Opt, base=THINKING, muted=True))
        else:
            for row in wrap_words(para, body - 2):
                b.line(RAIL + inline(row, Opt, base=THINKING, muted=True))


def tool_head(b, opt, ok, name, title, meta):
    """`• shell cargo test … · 0.4s`: the tool name in BLUE, the title in TEXT, meta DIM."""
    status = ("✓", GREEN) if ok else ("✗", RED)
    if opt.today:
        return
    b.line(mark("•") + [(name, BLUE, "", None), (" " + title, TEXT, "", None), (f" · {meta} ", DIM, "", None),
                        (status[0], status[1], "", None)])


def shell_card(b, opt):
    cmd = "cargo test -p radian-host --locked"
    out_rows = [
        "running 41 tests",
        "test job_notify::settled_job_wakes_the_idle_model ... ok",
        "test job_notify::a_waited_job_sends_no_notice ... ok",
        "test result: ok. 41 passed; 0 failed; 0 ignored",
    ]
    if opt.today:
        b.halfpad(True, CARD_BG)
        b.line([(" ✓ ", GREEN, "", None), ("shell cargo test", MUTED, "", None)], row_bg=CARD_BG)
        b.line([(" $ " + cmd, TEXT, "", None)], row_bg=CARD_BG)
        b.line([], row_bg=CARD_BG)
        for row in out_rows:
            b.line([(" " + row, TEXT, "", None)], row_bg=CARD_BG)
        b.line([], row_bg=CARD_BG)
        b.line([(" ⟦exit 0 | 12.4s | timeout 600s⟧", DIM, "", None)], row_bg=CARD_BG)
        b.halfpad(False, CARD_BG)
        return
    tool_head(b, opt, True, "shell", "$ " + cmd, "exit 0 · 12.4s")
    for row in out_rows:
        b.line(RAIL + [(row, MUTED, "", None)])


def read_line(b, opt):
    if opt.today:
        b.line([("  ✓ ", GREEN, "", None), ("read crates/radian-host/src/job_notify.rs (100-160)", MUTED, "", None)])
        return
    b.line(mark("•") + [("read", BLUE, "", None), (" crates/radian-host/src/job_notify.rs", TEXT, "", None),
                        (" 100-160 · 0.01s ", DIM, "", None), ("✓", GREEN, "", None)])


def patch_card(b, opt):
    if opt.today:
        b.halfpad(True, CARD_BG)
        b.line([(" ✓ ", GREEN, "", None), (f"patch {PATCH_PATH}", MUTED, "", None)], row_bg=CARD_BG)
        b.line([], row_bg=CARD_BG)
        for sign, text in DIFF:
            color = {"@": VIOLET, "+": GREEN, "-": RED, " ": MUTED}[sign]
            b.line([(" " + (text if sign == "@" else sign + text), color, "", None)], row_bg=CARD_BG)
        b.line([], row_bg=CARD_BG)
        b.line([(" ⟦0.03s⟧", DIM, "", None)], row_bg=CARD_BG)
        b.halfpad(False, CARD_BG)
        return
    tool_head(b, opt, True, "patch", PATCH_PATH, "+5 −2 · 0.03s")
    old = new = 120
    for sign, text in DIFF:
        if sign == "@":
            b.line(RAIL + [(text, DIM, "", None)])
            continue
        if sign == "+":
            num, sc, base = new, GREEN, TEXT
            new += 1
        elif sign == "-":
            num, sc, base = old, RED, TEXT
            old += 1
        else:
            num, sc, base = new, MUTED, MUTED
            old += 1
            new += 1
        if opt.numbers == "on":
            spans = RAIL + [(f"{num:>4} {sign} ", sc, "", None)]
            spans += highlight(text, base) if sign != " " else [(text, base, "", None)]
        else:
            spans = RAIL + [(sign + text, sc, "", None)]
        b.line(spans)


def ask_block(b, opt):
    """The ask_user block as the transcript shows it after the answer (ADR 0024 R1)."""
    # the same open frame as code, diff and thinking: the frame is in the marker column,
    # `╭` at col 1, the rows' text at col 3; closed boxes are for overlays only
    b.line([("╭─ ask " + "─" * max(0, b.cols - 6), DIM, "", None)])

    def row(spans):
        b.line([("│ ", DIM, "", None)] + spans)

    row([("How should the UI extension be loaded?", TEXT, "", None)])
    row([("◉ ", VIOLET, "", None), ("External process plugins", TEXT, "", None)])
    row([("  ↳ a separate program, JSON-RPC over stdio", DIM, "", None)])
    row([("○ ", DIM, "", None), ("Built-in Rust components", MUTED, "", None)])
    b.line([("╰──", DIM, "", None)])


def reply(b, opt):
    body = b.cols - 2
    blocks = [
        ("h2", "What I did"),
        ("ol", [
            "Added `background: true` to the **shell** tool and a job registry in `radian-host`.",
            "Ran the Verify commands; `cargo test --workspace --locked` passed with **0 warnings**.",
        ]),
        ("code", ("rust", [
            "fn notice(&self, job: &Settled) -> Message {",
            "    Message::user(body).with_meta(meta) // never a user turn",
            "}",
        ])),
        ("table", (["Command", "Result"], [
            ["`cargo clippy --workspace`", "ok, **0 warnings**"],
            ["`cargo test --workspace --locked`", "412 passed"],
        ])),
    ]
    first = True
    for kind, data in blocks:
        if not first:
            b.blank()
        lead = mark("●") if (first and not opt.today) else M
        first = False
        if kind == "h2":
            spans = list(lead)
            if opt.today or opt.marks == "on":
                spans.append(("## ", DIM, "", None))
            spans.append((data, TEXT if opt.today else VIOLET, BOLD, None))
            b.line(spans)
        elif kind == "ol":
            for n, item in enumerate(data, 1):
                marker = f"{n}. "
                for k, row in enumerate(wrap_words(item, body - 4)):
                    pre = M + ([(marker, VIOLET, "", None)] if k == 0 else [(" " * len(marker), TEXT, "", None)])
                    b.line(pre + inline(row, Opt))
        elif kind == "code":
            lang, rows = data
            if opt.today:
                b.line(M + [("```" + lang, DIM, "", None)])
                for row in rows:
                    b.line(M + highlight(row, TEXT))
                b.line(M + [("```", DIM, "", None)])
            else:
                b.line([("╭─ " + lang + " " + "─" * max(0, b.cols - 4 - len(lang)), DIM, "", None)])
                for row in rows:
                    b.line(RAIL + highlight(row, TEXT))
                b.line([("╰──", DIM, "", None)])
        elif kind == "table":
            head, rows = data
            widths = [max(width(strip_inline(r[c])) for r in [head] + rows) for c in range(2)]
            rule = lambda l, m, r: b.line(M + [(l + m.join("─" * (w + 2) for w in widths) + r, DIM, "", None)])

            def trow(cells, header):
                spans = M + [("│", DIM, "", None)]
                for c, cell in enumerate(cells):
                    pad = widths[c] - width(strip_inline(cell))
                    if opt.today:
                        spans.append((" " + strip_inline(cell) + " " * pad + " ", TEXT, "", None))
                    elif header:
                        spans.append((" " + cell + " " * pad + " ", VIOLET, BOLD, None))
                    else:
                        spans += [(" ", TEXT, "", None)] + inline(cell, Opt) + [(" " * pad + " ", TEXT, "", None)]
                    spans.append(("│", DIM, "", None))
                b.line(spans)
            rule("┌", "┬", "┐")
            trow(head, True)
            rule("├", "┼", "┤")
            for i, r in enumerate(rows):
                trow(r, False)
                if i + 1 < len(rows):
                    rule("├", "┼", "┤")
            rule("└", "┴", "┘")


def compaction(b, opt):
    text = "── context compacted (rev 2, 148k → 61k) ──"
    b.line([(text, DIM, "", None)])     # a rule is structural: it starts in the marker column


def notice(b, opt, text, warn=False):
    if warn:
        b.line(mark("!", YELLOW) + [(text, YELLOW, "", None)])
    else:
        label, rest = text.split(" ", 1)
        b.line(mark("*", DIM) + [(label, VIOLET, "", None), (" " + rest, MUTED, "", None)])


def transcript(b, opt):
    user_line(b, opt, "finish ADR 0037 slice 3 and show me the diff")
    b.blank()
    thinking(b, opt, folded=False)
    b.blank()
    shell_card(b, opt)
    b.blank()
    read_line(b, opt)
    b.blank()
    patch_card(b, opt)
    b.blank()
    ask_block(b, opt)
    b.blank()
    thinking(b, opt, folded=True)
    b.blank()
    reply(b, opt)
    b.blank()
    compaction(b, opt)
    b.blank()
    notice(b, opt, "task grep-glob finished · radian/t1 @ a1b2c3 · 3 files")
    notice(b, opt, "task docs stalled · no journal write for 6m", warn=True)


# ---- the `/` menu, the modals, the input and the footer ---------------------------------------
def slash_menu(b, opt):
    items = [("/model", "switch the model or provider"), ("/mode", "general · lead"),
             ("/memory", "search, add, edit"), ("/jobs", "background jobs"), ("/tasks", "task agents")]
    for i, (name, desc) in enumerate(items):
        cur = i == 0
        b.line([("❯ " if cur else "  ", VIOLET, "", None), (f"{name:<10}", TEXT if cur else MUTED, BOLD if cur else "", None),
                (desc, DIM, "", None)])


def panel_box(b, title, rows, hints):
    inner = b.cols - 4
    b.line([("╭─ " + title + " " + "─" * max(0, inner - width(title) - 1) + "╮", DIM, "", None)])
    for spans in rows:
        text = "".join(t for t, *_ in spans)
        b.line([("│ ", DIM, "", None)] + spans + [(" " * max(0, inner - width(text)) + " │", DIM, "", None)])
    hint = " · ".join(hints)
    b.line([("╰─ ", DIM, "", None), (hint, MUTED, "", None), (" " + "─" * max(0, inner - width(hint) - 1) + "╯", DIM, "", None)])


def approval_modal(b, opt):
    panel_box(b, "approve tool call?", [
        [("tool: ", MUTED, "", None), ("shell", TEXT, "", None)],
        [("command: ", MUTED, "", None), ("git push origin radian/integration", TEXT, "", None)],
    ], ["[y] allow once", "[a] always this session", "[n] deny", "[!] auto"])


def ask_modal(b, opt):
    panel_box(b, "Ask · lead", [
        [(" Load ", TEXT, BOLD, None), ("  Scope  Submit", MUTED, "", None)],
        [("How should the UI extension be loaded?", TEXT, "", None)],
        [("❯ ", VIOLET, "", None), ("○ Built-in Rust components", TEXT, "", None)],
        [("  ○ External process plugins", MUTED, "", None)],
        [("     ↳ a separate program, JSON-RPC over stdio", DIM, "", None)],
        [("  ○ Other (type your own)", MUTED, "", None)],
    ], ["⏎ select", "n note", "↑/↓ move", "⇥ question", "esc cancel"])


def input_rows(b, opt):
    status = [("◜", VIOLET, "", None), ("  Working · 12s", DIM, "", None), (" · esc to interrupt", DIM, "", None)]
    prompt = [("❯ ", VIOLET, "", None), ("explain the inbox lanes", TEXT, "", None), ("▏", TEXT, "", None)]
    if opt.input == "bar":
        # today's default (ADR 0035 V3 / rev 3 / rev 4): a status row, then a band with the `▌` rail
        b.line([("  ", TEXT, "", None)] + status)
        b.halfpad(True, USER_BG, rail=ACCENT_BAR)
        b.line([("▌ ", ACCENT_BAR, "", None)] + prompt[1:], row_bg=USER_BG)
        b.halfpad(False, USER_BG, rail=ACCENT_BAR)
        return
    if opt.input == "lines":
        # today's `lines` style: a rule above (the spinner on it, ADR 0027) and a rule below
        b.line([("── ", DIM, "", None)] + status + [(" " + "─" * b.cols, DIM, "", None)])
        b.line(prompt)
        b.line([("─" * b.cols, DIM, "", None)])
        return
    # thin: one rule above carrying the status, the prompt, nothing below
    b.line([("── ", DIM, "", None)] + status + [(" " + "─" * b.cols, DIM, "", None)])
    b.line(prompt)


def footer_rows(b, opt):
    sep = (" │ ", DIM, "", None)
    b.line([("Usage: 5h: 80% · 7d: 19% · 30d: 40% · 5h ↺1h35m", DIM, "", None)])
    left = [(" glm-5.3-flash", TEXT, "", None), sep, (" radian", MUTED, "", None), sep,
            (" adr/0030-rev2 +2", MUTED, "", None), sep, (" render-study", MUTED, "", None)]
    right = [("▣ ━━━━━━━━──────── 41%", MUTED, "", None), ("  ◐ high", VIOLET, "", None)]
    pad = max(1, b.cols - width("".join(t for t, *_ in left)) - width("".join(t for t, *_ in right)))
    b.line(left + [(" " * pad, TEXT, "", None)] + right)
    nums = "↑15M ↓2.6M R1962M $14.510 41.3%/1.0M"
    b.line([(" " * max(0, b.cols - width(nums)), TEXT, "", None), (nums, DIM, "", None)])


# ---- the orchestration side panel (ADR 0030 O10: agent board + kanban + dashboard, folded) ----
def side_panel(b, opt):
    def head(text):
        b.line([(text, VIOLET, BOLD, None)])

    head("agents")
    rows = [("●", VIOLET, "lead", "running", "12s", "you"),
            ("●", VIOLET, "grep-glob", "running", "4m12s", "3 files"),
            ("◐", YELLOW, "docs", "waiting", "approval", ""),
            ("✓", GREEN, "tests", "done", "a1b2c3", "+412"),
            ("◌", DIM, "bg-jobs", "queued", "", "")]
    for glyph, color, name, state, t, extra in rows:
        b.line([(glyph + " ", color, "", None), (f"{name:<10}", TEXT if state == "running" else MUTED, "", None),
                (f"{state:<8}", DIM, "", None), (f"{t:<7}", DIM, "", None), (extra, DIM, "", None)])
    b.blank()
    head("pending")
    b.line([("◐ ", YELLOW, "", None), ("docs", TEXT, "", None), (" · approve git push", MUTED, "", None)])
    b.line([("? ", VIOLET, "", None), ("lead", TEXT, "", None), (" · ask: how to load", MUTED, "", None)])
    b.blank()
    head("usage")
    for name, tok, cost, ctx in [("lead", "↑1.2M ↓48k", "$3.10", "41%"), ("grep-glob", "↑402k ↓19k", "$0.92", "63%"),
                                 ("docs", "↑88k ↓4k", "$0.20", "12%"), ("tests", "↑610k ↓31k", "$1.44", "—")]:
        b.line([(f"{name:<10}", MUTED, "", None), (f"{tok:<12}", DIM, "", None), (f"{cost:>6}", DIM, "", None),
                (f" {ctx:>4}", DIM, "", None)])
    b.blank()
    b.line([("heavy 1/2 · tasks 3/4", DIM, "", None)])


# ---- compose the frame -----------------------------------------------------------------------
def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--width", type=int, default=110)
    ap.add_argument("--panel", choices=["on", "off"], default="on")
    ap.add_argument("--menu", choices=["on", "off"], default="off")
    ap.add_argument("--modal", choices=["none", "approval", "ask"], default="none")
    ap.add_argument("--input", choices=["thin", "bar", "lines"], default="thin")
    ap.add_argument("--marks", choices=["on", "off"], default="off")
    ap.add_argument("--numbers", choices=["on", "off"], default="on")
    ap.add_argument("--today", action="store_true")
    ap.add_argument("--theme", default="dark", help="dark | light | a pi theme name or file")
    opt = ap.parse_args()
    label = apply_theme(opt.theme)
    if PAGE_BG:
        sys.stdout.write(bg(PAGE_BG) + "\x1b[2J\x1b[H")

    panel_w = 34 if opt.panel == "on" else 0
    main_w = opt.width - panel_w - (3 if panel_w else 0)

    t = Buf(main_w)
    transcript(t, opt)
    t.blank()
    bottom = Buf(main_w)
    if opt.menu == "on":
        slash_menu(bottom, opt)
    if opt.modal == "approval":
        approval_modal(bottom, opt)
    elif opt.modal == "ask":
        ask_modal(bottom, opt)
    else:
        input_rows(bottom, opt)
    bottom.blank()
    footer_rows(bottom, opt)

    side = Buf(panel_w) if panel_w else None
    if side:
        side_panel(side, opt)

    main_rows = t.rows + bottom.rows
    total = len(main_rows)
    out = []
    for i in range(total):
        spans, row_bg = main_rows[i]
        line = render_row_bg(spans, row_bg, main_w)
        if side:
            s_spans, s_bg = side.rows[i] if i < len(side.rows) else ([], None)
            divider = f"{RESET}{bg(PAGE_BG)}{fg(DIM)} │ {RESET}"
            line += divider + render_row_bg(s_spans, s_bg, panel_w)
        out.append(line)
    sys.stdout.write("\n".join(out) + "\n")
    sys.stdout.write(f"{RESET}{bg(PAGE_BG)}{fg(DIM)}{'':<{opt.width}}{RESET}\n{RESET}{fg(DIM)}theme: {label}{RESET}\n")


if __name__ == "__main__":
    main()

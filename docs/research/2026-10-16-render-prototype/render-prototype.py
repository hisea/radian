#!/usr/bin/env python3
"""radian transcript rendering prototype (2026-10-16).

Prints one short transcript with ANSI truecolor in radian's built-in dark palette (pi dark;
the okhsl values converted with the same algorithm as `theme/color.rs`):

  a user message, an expanded thinking block, a `patch` tool card with a unified diff,
  a folded thinking line, and an assistant Markdown reply (heading, numbered list with bold
  and inline code, a fenced code block, a table).

Three looks:

  --today          what radian draws now, reconstructed from `blocks.rs` / `render.rs` /
                   `tools.rs` at origin/main 5836485 (after ADR 0028, ADR 0035)
  --style rule     the proposal: no background bands; columns 1-2 are the marker column and
                   hold the marker OR a thin rail (thinking, code, diff); every body row
                   starts at column 3 (default)
  --style band     the proposal, opencode-like: raised-background cards and a `▌` rail

Default prints today, then the proposal. The open points are switches, so each alternative
can be looked at in the terminal:

  --marks on|off     keep the `#` heading marks (today: on, ADR 0035 decision 3) / conceal
  --marker dot|blank assistant rows start with `●` (dsh) / a blank marker column (today)
  --bold plain|warm  bold text in the text colour / in a warm accent (opencode)
  --code text|box    inline code as coloured text / coloured text on a raised background
  --header plain|bold table header cells plain / in the heading style
  --rail on|off      thinking body behind a left rule / plain indent
  --numbers on|off   diff rows carry a line-number gutter / bare `+`/`-` rows
  --tint on|off      added/removed diff rows get a full-width background tint
  --width N          transcript width (default 80)

Nothing here touches radian; it is a picture to decide from.
"""
import argparse
import sys
import unicodedata

# ---- palette: radian built-in dark (pi dark), theme.rs `mod pi` -----------------------
TEXT = (222, 224, 225)
MUTED = (157, 165, 169)
DIM = (126, 136, 142)
VIOLET = (167, 152, 215)        # accent: headings, bullets, hunk headers
BLUE = (105, 173, 208)          # syntax keyword; dsh's tool name colour is in this family
STRING = (222, 141, 90)         # syntax string
GREEN = (104, 183, 141)
RED = (234, 127, 129)
YELLOW = (205, 154, 34)         # md_code today
THINKING = (150, 160, 164)
ACCENT_BAR = (160, 142, 213)    # bar rail / user message rail
USER_BG = (33, 59, 73)
CARD_BG = (52, 56, 58)          # tool_*_bg after ADR 0028: one neutral raised colour
ADD_BG = (37, 65, 49)
DEL_BG = (91, 40, 42)
ADD_GUTTER = (30, 54, 41)
DEL_GUTTER = (74, 33, 35)
CTX_GUTTER = (44, 48, 50)
WARM = (245, 167, 66)           # opencode's `markdown.strong`, for --bold warm
CODE_BOX = (66, 70, 72)         # raised background for --code box

RESET = "\x1b[0m"
BOLD = "\x1b[1m"
ITALIC = "\x1b[3m"
UNDER = "\x1b[4m"


def fg(c):
    return f"\x1b[38;2;{c[0]};{c[1]};{c[2]}m"


def bg(c):
    return f"\x1b[48;2;{c[0]};{c[1]};{c[2]}m" if c else ""


def width(s):
    w = 0
    for ch in s:
        if unicodedata.combining(ch):
            continue
        w += 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
    return w


class Out:
    def __init__(self, cols):
        self.cols = cols

    def line(self, spans, row_bg=None):
        """spans: (text, fg, attrs, span_bg). Pads to the full width with row_bg."""
        buf = []
        used = 0
        for text, color, attrs, span_bg in spans:
            b = span_bg if span_bg else row_bg
            # code rows never wrap (ADR 0028 R9): clip at the width like the TUI does
            while text and used + width(text) > self.cols:
                text = text[:-1]
            if not text:
                continue
            buf.append(f"{RESET}{bg(b)}{fg(color)}{attrs}{text}")
            used += width(text)
        pad = max(0, self.cols - used)
        buf.append(f"{RESET}{bg(row_bg)}{' ' * pad}{RESET}")
        sys.stdout.write("".join(buf) + "\n")

    def blank(self):
        sys.stdout.write(RESET + "\n")

    def halfpad(self, top, color, rail=None):
        """ADR 0035/4: a half-cell pad row, `▄` above / `▀` below, fg = the band colour."""
        glyph = "▄" if top else "▀"
        row = glyph * self.cols
        if rail:
            corner = "▖" if top else "▘"
            sys.stdout.write(f"{RESET}{fg(rail)}{corner}{fg(color)}{row[1:]}{RESET}\n")
        else:
            sys.stdout.write(f"{RESET}{fg(color)}{row}{RESET}\n")


# ---- inline markdown: `code`, **bold**, *italic* ------------------------------------------
def inline(text, opt, base=TEXT, base_attr="", muted=False):
    """`muted` keeps bold/italic and drops the colours (opencode's thinking trick)."""
    spans = []
    i = 0
    buf = ""

    def flush():
        nonlocal buf
        if buf:
            spans.append((buf, base, base_attr, None))
            buf = ""

    while i < len(text):
        if text.startswith("**", i) and text.find("**", i + 2) != -1:
            j = text.find("**", i + 2)
            flush()
            color = base if muted else (WARM if opt.bold == "warm" else TEXT)
            spans.append((text[i + 2:j], color, base_attr + BOLD, None))
            i = j + 2
        elif text[i] == "`" and text.find("`", i + 1) != -1:
            j = text.find("`", i + 1)
            flush()
            inner = text[i + 1:j]
            if muted:
                spans.append((inner, base, base_attr, None))
            elif opt.code == "box":
                spans.append((inner, YELLOW, base_attr, CODE_BOX))
            else:
                spans.append((inner, YELLOW, base_attr, None))
            i = j + 1
        elif text[i] == "*" and text.find("*", i + 1) != -1:
            j = text.find("*", i + 1)
            flush()
            spans.append((text[i + 1:j], base, base_attr + ITALIC, None))
            i = j + 1
        else:
            buf += text[i]
            i += 1
    flush()
    return spans


def wrap_words(text, cols):
    out, line = [], ""
    for word in text.split(" "):
        if line and width(line) + 1 + width(word) > cols:
            out.append(line)
            line = word
        else:
            line = word if not line else line + " " + word
    if line:
        out.append(line)
    return out


def strip_inline(s):
    return s.replace("`", "").replace("**", "")


# ---- content --------------------------------------------------------------------------------
USER_TEXT = "finish ADR 0037 slice 3 and show me the diff"

THOUGHT = [
    "**Checking the inbox lanes first.**",
    "",
    "The job notice must not lower the lead's turn: `observe_input_source` only ever lowers the source, so an `inject` from a System job would turn a human turn into a non-human one.",
    "",
    "- `followup` wakes a new turn, so the source is set fresh",
    "- `inject` lands mid-turn and lowers it",
    "",
    "So the completion should return through `wait_task`, not through a notice.",
]
THOUGHT_SECS = "3.2s"

PATCH_PATH = "crates/radian-host/src/job_notify.rs"
DIFF = [
    ("@", "@@ -120,9 +120,12 @@ impl JobNotify {"),
    (" ", "    fn notice(&self, job: &Settled) -> Message {"),
    ("-", "        let source = InputSource::User;"),
    ("+", "        // ADR 0037: a notice is low-trust data, never a user turn."),
    ("+", "        let source = InputSource::System;"),
    (" ", "        let mut meta = MessageMeta::default();"),
    (" ", "        meta.source = Some(source);"),
    ("-", "        Message::user(format!(\"job {} done\", job.id)).with_meta(meta)"),
    ("+", "        let body = format!(\"<job id={} exit={} />\", job.id, job.exit);"),
    ("+", "        Message::user(body).with_meta(meta)"),
    (" ", "    }"),
]

REPLY = [
    ("h2", "What I did"),
    ("ol", [
        "Added `background: true` to the **shell** tool and a job registry in `radian-host`.",
        "Wrote `job_output`, `job_list` and `job_kill`; a settled job wakes the model at most 3 times in a row.",
        "Ran the Verify commands; `cargo test --workspace --locked` passed with **0 warnings**.",
    ]),
    ("code", ("rust", [
        "fn notice(&self, job: &Settled) -> Message {",
        "    let body = format!(\"<job id={} exit={} />\", job.id, job.exit);",
        "    Message::user(body).with_meta(meta) // never a user turn",
        "}",
    ])),
    ("h2", "Result: all green."),
    ("table", (["Command", "Result"], [
        ["`cargo fmt --all -- --check`", "ok"],
        ["`cargo clippy --workspace`", "ok, **0 warnings**"],
        ["`cargo test --workspace --locked`", "412 passed"],
    ])),
    ("p", "The foreground timeout message now names `background: true`, so the model stops polling."),
]

KEYWORDS = {"fn", "let", "mut", "impl", "return", "use"}


def highlight(code, base):
    """A crude Rust highlight for the picture: keywords blue, strings warm, comments dim."""
    spans = []
    stripped = code.lstrip(" ")
    indent = code[: len(code) - len(stripped)]
    if indent:
        spans.append((indent, base, "", None))
    if stripped.startswith("//"):
        spans.append((stripped, DIM, ITALIC, None))
        return spans
    i, token = 0, ""
    while i < len(stripped):
        ch = stripped[i]
        if ch == '"':
            if token:
                spans.append((token, BLUE if token in KEYWORDS else base, "", None))
                token = ""
            j = i + 1
            while j < len(stripped):
                if stripped[j] == "\\":
                    j += 2
                    continue
                if stripped[j] == '"':
                    break
                j += 1
            spans.append((stripped[i:j + 1], STRING, "", None))
            i = j + 1
            continue
        if ch == "/" and stripped.startswith("//", i):
            if token:
                spans.append((token, BLUE if token in KEYWORDS else base, "", None))
                token = ""
            spans.append((stripped[i:], DIM, ITALIC, None))
            return spans
        if ch.isalnum() or ch == "_":
            token += ch
        else:
            if token:
                spans.append((token, BLUE if token in KEYWORDS else base, "", None))
                token = ""
            spans.append((ch, base, "", None))
        i += 1
    if token:
        spans.append((token, BLUE if token in KEYWORDS else base, "", None))
    return spans


# ---- user message --------------------------------------------------------------------------
def render_user(out, opt, today):
    if today or opt.style == "band":
        # ADR 0028 / ADR 0035: a band with the accent `▌` rail
        out.halfpad(True, USER_BG, rail=ACCENT_BAR)
        out.line([("▌ ", ACCENT_BAR, "", None), (USER_TEXT, TEXT, "", None)], row_bg=USER_BG)
        out.halfpad(False, USER_BG, rail=ACCENT_BAR)
    else:
        # dsh: `❯` and the text in the accent colour, no band
        out.line([("❯ ", VIOLET, "", None), (USER_TEXT, VIOLET, "", None)])


# ---- thinking ------------------------------------------------------------------------------
def render_thinking(out, opt, today, folded):
    body = out.cols - 2
    if today:
        if folded:
            out.line([("▸ ", THINKING, "", None), ("thinking (8 lines)", THINKING, ITALIC, None)])
            return
        first = True
        for para in THOUGHT:
            for row in (wrap_words(para, body) if para else [""]):
                out.line([("~ " if first else "  ", THINKING, "", None), (row, THINKING, ITALIC, None)])
                first = False
        return
    if folded:
        out.line([("▸ ", THINKING, "", None), (f"thinking · {THOUGHT_SECS} · 8 lines", THINKING, "", None)])
        return
    out.line([("▾ ", THINKING, "", None), (f"thinking · {THOUGHT_SECS}", THINKING, "", None)])
    if opt.rail == "off":
        lead = [("  ", TEXT, "", None)]
    elif opt.style == "band":
        lead = [("▌ ", DIM, "", None)]      # the `▌` family (user message, bar input), weaker
    else:
        lead = [("│ ", DIM, "", None)]      # the thin rule family (code blocks, diff)
    for para in THOUGHT:
        if para == "":
            out.line(lead + [("", THINKING, "", None)])
            continue
        if para.startswith("- "):
            for k, row in enumerate(wrap_words(para[2:], body - 4)):
                out.line(lead + [("- " if k == 0 else "  ", THINKING, "", None)]
                         + inline(row, opt, base=THINKING, muted=True))
            continue
        for row in wrap_words(para, body - 2):
            out.line(lead + inline(row, opt, base=THINKING, muted=True))


# ---- patch tool with a diff -------------------------------------------------------------------
def diff_rows(out, opt, today, lead_fn, row_bg_fn):
    """Emits the diff rows. lead_fn(sign) -> the cells before the code; row_bg_fn(sign) -> bg."""
    old = new = 120
    for sign, text in DIFF:
        if sign == "@":
            out.line(lead_fn("@") + [(text, VIOLET if today else DIM, "", None)], row_bg=row_bg_fn(" "))
            continue
        if sign == "+":
            num, gutter, sign_color, base = new, ADD_GUTTER, GREEN, TEXT
            new += 1
        elif sign == "-":
            num, gutter, sign_color, base = old, DEL_GUTTER, RED, TEXT
            old += 1
        else:
            num, gutter, sign_color, base = new, CTX_GUTTER, MUTED, MUTED
            old += 1
            new += 1
        row_bg = row_bg_fn(sign)
        if today or opt.numbers == "off":
            color = {"+": GREEN, "-": RED, " ": MUTED}[sign]
            out.line(lead_fn(sign) + [(sign + text, color, "", None)], row_bg=row_bg)
            continue
        # opencode's unified gutter: one number column (new number for `+`/context, old for
        # `-`), then ` +`/` -`, then the code; the gutter keeps its own colour on tinted rows
        gutter_bg = gutter if (opt.tint == "on" and opt.style == "band") else None
        spans = lead_fn(sign) + [(f"{num:>4} {sign} ", sign_color, "", gutter_bg)]
        if today or opt.style == "band" or sign != " ":
            spans.extend(highlight(text, base))
        else:
            spans.append((text, base, "", None))
        out.line(spans, row_bg=row_bg)


def render_patch(out, opt, today):
    if today or opt.style == "band":
        pad = " "
        out.halfpad(True, CARD_BG)
        out.line([(pad + "✓ ", GREEN, "", None), (f"patch {PATCH_PATH}", MUTED, "", None)], row_bg=CARD_BG)
        out.line([], row_bg=CARD_BG)

        def row_bg(sign):
            if today or opt.tint == "off" or sign == " ":
                return CARD_BG
            return ADD_BG if sign == "+" else DEL_BG

        diff_rows(out, opt, today, lambda sign: [(pad, TEXT, "", None)], row_bg)
        out.line([], row_bg=CARD_BG)
        out.line([(pad + "⟦0.03s⟧", DIM, "", None)], row_bg=CARD_BG)
        out.halfpad(False, CARD_BG)
        return
    # dsh-like: one head line `• patch path · 0.03s`, the diff under a thin left rule
    out.line([("• ", DIM, "", None), ("patch", BLUE, "", None), (f" {PATCH_PATH}", TEXT, "", None),
              (" · 0.03s", DIM, "", None)])

    def row_bg(sign):
        if opt.tint == "on" and sign in "+-":
            return ADD_BG if sign == "+" else DEL_BG
        return None

    diff_rows(out, opt, today, lambda sign: [("│ ", DIM, "", None)], row_bg)


# ---- assistant markdown ---------------------------------------------------------------------
def render_reply(out, opt, today):
    margin = [("  ", TEXT, "", None)]
    body = out.cols - 2
    first = True
    for kind, data in REPLY:
        if not first:
            out.blank()               # ADR 0035 V1: exactly one blank line between blocks
        lead = [("● ", DIM, "", None)] if (first and not today and opt.marker == "dot") else margin
        entry_start = first
        first = False
        if kind == "h2":
            # (the short rule above dsh's headings in the owner's screenshot was a Markdown
            # `---`, dsh-TUI `markdown.ts:438`: three dashes; not a heading feature)
            spans = list(lead)
            if today or opt.marks == "on":
                spans.append(("## ", DIM, "", None))
            spans.append((data, TEXT if today else VIOLET, BOLD, None))
            out.line(spans)
        elif kind == "p":
            for k, row in enumerate(wrap_words(data, body)):
                out.line((lead if k == 0 else margin) + inline(row, opt))
        elif kind == "ol":
            for n, item in enumerate(data, 1):
                marker = f"{n}. "
                for k, row in enumerate(wrap_words(item, body - len(marker))):
                    pre = (lead if (k == 0 and n == 1) else margin)
                    pre = pre + ([(marker, VIOLET, "", None)] if k == 0 else [(" " * len(marker), TEXT, "", None)])
                    out.line(pre + inline(row, opt))
        elif kind == "code":
            lang, rows = data
            if today or opt.style == "band":
                # today: the fence rows stay, dim (ADR 0035 table), code at the same margin
                out.line(margin + [("```" + lang, DIM, "", None)])
                for row in rows:
                    out.line(margin + highlight(row, TEXT))
                out.line(margin + [("```", DIM, "", None)])
            else:
                # the frame lives in the marker column (cols 1-2), the code starts at col 3
                # like every other body row: a top rule carrying the language, a thin left
                # rule, a short bottom rule
                top = "╭─ " + lang + " " + "─" * max(0, out.cols - 4 - len(lang))
                out.line([(top, DIM, "", None)])
                for row in rows:
                    out.line([("│ ", DIM, "", None)] + highlight(row, TEXT))
                out.line([("╰──", DIM, "", None)])
        elif kind == "table":
            render_table(out, opt, today, lead, margin, *data)


def render_table(out, opt, today, lead, margin, head, rows):
    cols = len(head)
    widths = [max(width(strip_inline(r[c])) for r in [head] + rows) for c in range(cols)]
    border = DIM

    def rule(l, m, r, pre):
        out.line(pre + [(l + m.join("─" * (w + 2) for w in widths) + r, border, "", None)])

    def row(cells, header):
        spans = margin + [("│", border, "", None)]
        for c, cell in enumerate(cells):
            pad = widths[c] - width(strip_inline(cell))
            if today:
                # today: md_table concatenates piece text, so inline styles are lost
                spans.append((" " + strip_inline(cell) + " " * pad + " ", TEXT, "", None))
            elif header and opt.header == "bold":
                spans.append((" " + strip_inline(cell) + " " * pad + " ", VIOLET, BOLD, None))
            else:
                spans.append((" ", TEXT, "", None))
                spans.extend(inline(cell, opt))
                spans.append((" " * pad + " ", TEXT, "", None))
            spans.append(("│", border, "", None))
        out.line(spans)

    rule("┌", "┬", "┐", lead)
    row(head, True)
    rule("├", "┼", "┤", margin)
    for i, r in enumerate(rows):
        row(r, False)
        if i + 1 < len(rows):
            rule("├", "┼", "┤", margin)   # ADR 0035: a rule between every two rows
    rule("└", "┴", "┘", margin)


# ---- the transcript ---------------------------------------------------------------------------
def section(out, title):
    out.blank()
    out.line([(f"── {title} ", DIM, "", None), ("─" * max(0, out.cols - width(title) - 4), DIM, "", None)])
    out.blank()


def render_all(out, opt, today):
    label = "today (origin/main, reconstructed)" if today else f"proposal · style={opt.style}"
    section(out, label)
    render_user(out, opt, today)
    out.blank()
    render_thinking(out, opt, today, folded=False)
    out.blank()                       # ADR 0035: one blank line after thinking
    render_patch(out, opt, today)
    out.blank()
    render_thinking(out, opt, today, folded=True)
    out.blank()
    render_reply(out, opt, today)
    out.blank()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--width", type=int, default=80)
    ap.add_argument("--today", action="store_true", help="only today's look")
    ap.add_argument("--new", action="store_true", help="only the proposal")
    ap.add_argument("--style", choices=["rule", "band"], default="rule")
    ap.add_argument("--marks", choices=["on", "off"], default="off")
    ap.add_argument("--marker", choices=["dot", "blank"], default="dot")
    ap.add_argument("--bold", choices=["plain", "warm"], default="plain")
    ap.add_argument("--code", choices=["text", "box"], default="text")
    ap.add_argument("--header", choices=["plain", "bold"], default="bold")
    ap.add_argument("--rail", choices=["on", "off"], default="on")
    ap.add_argument("--numbers", choices=["on", "off"], default="on")
    ap.add_argument("--tint", choices=["on", "off"], default=None)
    opt = ap.parse_args()
    if opt.tint is None:
        opt.tint = "on" if opt.style == "band" else "off"
    out = Out(opt.width)
    both = not (opt.today or opt.new)
    if opt.today or both:
        render_all(out, opt, today=True)
    if opt.new or both:
        render_all(out, opt, today=False)


if __name__ == "__main__":
    main()

---
name: theme-import
description: Bring a pi theme into Radian, or write and tune a Radian theme by hand from a color palette or a screenshot. Use when asked to import a pi theme, to set up or change Radian's colors, to make Radian look like another editor or terminal, or to fix theme contrast (text levels, semantic colors).
---

# Radian themes

Radian's theme is a TOML file at `~/.radian/themes/<name>.toml`, selected with
`tui.theme = "<name>"` in `~/.radian/config/config.toml`. The built-ins are `dark`, `light`
and `mono`; `NO_COLOR` forces `mono` and wins over every file and override.

## From a pi theme: use the command, do not hand-write

If the user has a pi theme JSON (say `~/.pi/agent/themes/tokyo-night.json`), the mapping is a
fixed table, so a host command does it. Run it; do not transcribe colors by hand.

```
radian theme import <pi-theme.json> [--name <name>] [--force]
```

- It prints `wrote ~/.radian/themes/<name>.toml`, then a report: every pi key it dropped and
  every role that kept the built-in value. Read the report; a long fallback or dropped list is
  not a failure, only information.
- Without `--name` it uses the file's own `name` field, else the file stem.
- It refuses to overwrite an existing `<name>.toml` unless you pass `--force`. Ask the user
  before you pass `--force`.
- Then set `tui.theme = "<name>"` (or run `/theme <name>` in the TUI, which also writes it).

## By hand, from a palette or a screenshot

When there is no pi file, write the TOML yourself. Read the colors off the palette or
screenshot and assign them by *meaning*, not by picking a nice color for each role.

```toml
name = "tokyo-night"
appearance = "dark"          # dark | light; picks which built-in the unset roles keep

[vars]                       # optional, reusable colors
accent = "#7aa2f7"
bg = "#1a1b26"

[roles]
text = "#c0caf5"             # a bare color is the foreground only
user = "$accent"             # $name references a [vars] entry (chains allowed)
selection = { bg = "#283457" }
frame = { fg = "$accent", dim = true }
```

- Keys are Radian role names (snake_case). A value is a color string, or a table
  `{ fg, bg, bold, italic, dim, underline }`. `fg`/`bg` take the same color forms as below;
  the booleans force a modifier on (`true`) or off (`false`).
- Colors: `#rgb` / `#rrggbb`, `oklch(…)`, `okhsl(…)`, a `0`–`255` palette index, `$var`, or
  `""` for the terminal's default color.
- An unknown role name, a bad value or a missing/looping variable is one warning line; only
  that role falls back to the built-in palette chosen by `appearance`. Nothing blocks startup.

### The roles that matter most

| Role | What it colors | Pick |
|---|---|---|
| `text` | the main level: answers, tool titles | the palette's brightest readable text |
| `tool_result`, `notice_body` | the secondary level: tool output, notice bodies | one step dimmer than `text` |
| `hint`, `truncation`, `frame`, `diff_line_number`, `footer_separator` | the weak level: hints, counts, structure | the dimmest still-readable color |
| `user` | the `❯` prompt and the user's words | the accent |
| `accent`-fed roles: `model`, `mcp`, `find`, `md_list_bullet`, `notice_label` | names and labels | the accent |
| `turn_idle`/`status_ok` (green), `note`/`status_warn`/`budget_over` (yellow), `tool_result_error`/`status_error` (red) | success / warning / error | the palette's semantic colors |
| `tool_call`, `tool_name` | a tool block's title and name | a code/keyword color |
| `diff_added` / `diff_removed` / `diff_context` / `diff_hunk` | diff lines and hunk headers | green / red / muted / weak |
| `md_*` and `syntax_*` | Markdown and code-block highlighting | follow the palette's own roles |

### Keep the three text levels apart

Radian's readability rule (ADR 0029) is main / secondary / weak. `text` must be the
brightest, the secondary roles one step down, the weak roles dimmer still. A palette whose
`text` and `muted` are nearly equal reads flat; pull them apart (see the tuning section). In a
light `appearance`, invert the direction: dark text, lighter weak colors.

## Tuning without editing the file

`[tui.colors]` in `~/.radian/config/config.toml` overrides any theme, per role, and is the
right place for a small nudge:

```toml
[tui.colors]
text = "#e6e6e6"
tool_result = { fg = "#9a9a9a" }
frame = { dim = true }
user = "#7aa2f7"
```

- Keys are Radian role names (pi role names are also accepted this version).
- A bare color is the foreground; a table may set `fg`, `bg` and the modifier forces.
- It is one warning line per unknown or malformed entry, and it never blocks startup.

Edit the theme file itself when the change belongs to the theme for everyone; use
`[tui.colors]` when it is this user's preference layered on top.

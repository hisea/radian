---
description: Write a color theme from scratch, import one, and shape the status line.
---

# Themes and the footer

A theme changes colors only, never behavior. The footer changes the status
line. Both come from files; both warn and fall back instead of refusing
startup.

## Choosing a theme

Type `/theme <name>` to switch now. Radian writes the name back to
`tui.theme` in the config file (`configuration.md` holds the path), so the
next session starts there. A read-only config applies for the session but is
never written.

Three names are built in: `dark`, `light` and `mono`. Any other name is a
file in `~/.radian/themes/`, named by its stem: `/theme tokyo-night` reads
`tokyo-night.toml`. Both `.toml` and `.json` files read; a `.toml` file wins
when both exist for one stem. The `.json` shape is the old format and loads
with a startup hint to convert it. An unknown name warns and keeps the
current theme.

`dark` is the original palette. `light` re-picks the same roles for a light
background. `mono` uses no color at all: bold, underline and dim only. A
non-empty `NO_COLOR` environment variable forces `mono`: `/theme` then does
nothing and says so.

Set the startup theme in the config file:

```toml
[tui.theme]
```

```toml
tui.theme = "dark"
```

## The theme file format

A theme file is TOML:

```toml
name = "tokyo-night"
appearance = "dark"   # dark | light; default dark

[vars]
accent = "#7aa2f7"
bg_raised = "okhsl(229 5% 24%)"

[roles]
text = "#c0caf5"
user = "$accent"                 # one color: the foreground only
selection = { bg = "#283457" }
frame = { fg = "$accent", dim = true }
```

`name` is informational; the configured name decides which file loads. A
missing `name` is tolerated. `appearance` picks which built-in palette the
roles you do not set keep: `dark` or `light`, default `dark`. An unknown
value warns and assumes `dark`. There is no inheritance beyond this: a file
starts from one built-in palette, then sets roles.

`[vars]` holds reusable colors. A role value of `$name` uses the entry, and
entries may reference each other with `$other`: chains resolve, and a loop
only breaks the roles that use it. A missing variable warns once per name.

A `[roles]` key is one of the 82 role names below. A value is one color for
the foreground, or a table with `fg`, `bg`, `bold`, `italic`, `dim` and
`underline`. An absent field leaves the built-in value alone; `bold = false`
turns off a bold the built-in sets. The same value shape overrides single
roles from the config file:

```toml
[tui.colors]
user = "#0af"
notice = { fg = "oklch(62% 0.1 200)", bold = true }
md_heading = 203
```

A color is one of: `#rgb`, `#rrggbb`, `oklch(L C H)`, `okhsl(H S L)`, a
`0`–`255` palette index, `$var`, or `""` for the terminal default. The
lightness in `oklch()` reads as a percentage or a plain `0`–`1` number; the
hue reads in degrees with an optional `deg` suffix. Out-of-gamut `oklch`
clips by reducing chroma and keeping the hue. With truecolor (`COLORTERM` is
`truecolor` or `24bit`) colors render as RGB; otherwise each renders as the
nearest 256-color entry.

Validation is forgiving on purpose. An unknown role name warns once and is
ignored. A bad value, a missing variable or a loop warns once and only the
affected roles fall back to the built-in palette chosen by `appearance`. A
file that does not parse, has no table at the top, or has no `[roles]` table
falls back as a whole. Nothing here blocks startup.

## Every role

Roles are the same keys `[tui.colors]` accepts. A missing role keeps the
built-in value.

Transcript text:

| Role | What it paints |
|---|---|
| `text` | The main text level. |
| `user` | Your messages. |
| `reasoning` | The body of the thinking channel. |
| `interrupted` | The `[interrupted]` marker and the cut-off body text. |
| `tool_call` | The tool name and title in a tool head line. |
| `tool_name` | The tool name inside the head line. |
| `tool_result` | A tool result body. |
| `tool_result_error` | A failed tool result. |
| `notice` | Transcript notices. |
| `notice_label` | The `label:` half of a notice line. |
| `notice_body` | The value half of a notice line. |
| `hint` | Hints: the placeholder, the question-form hint row, the picker help. |
| `trust_hint` | The project trust line under the wordmark. Only this line uses the role. |
| `completion` | The completion menu. |
| `find` | The in-transcript search match highlight. |
| `selection` | The transcript selection. A background only: the text keeps its own foreground. |
| `truncation` | Truncation markers. |
| `note` | Notes such as the startup Tip line. |
| `turn_idle` | The idle turn marker. |
| `turn_running` | The running turn marker. |
| `turn_cancelled` | The cancelled turn marker. |
| `approval_border` | The approval prompt border. |
| `frame` | Structural rules: the `│` rails, the `╭─`/`╰──` open frames, table rules, the compaction rule. |
| `diff_line_number` | A diff's gutter line numbers. |
| `status_ok` | A tool head's `✓`. |
| `status_warn` | A tool head's warning marker. |
| `status_error` | A tool head's `×`. |
| `auto` | The `AUTO` status marker. |

Per-level thinking colors (the footer's `reasoning` item paints one marker
per level so levels read apart at a glance):

| Role | What it paints |
|---|---|
| `reasoning_off` | The `off` level marker. |
| `reasoning_minimal` | The `minimal` level marker. |
| `reasoning_low` | The `low` level marker. |
| `reasoning_medium` | The `medium` level marker. |
| `reasoning_high` | The `high` level marker. |
| `reasoning_xhigh` | The `xhigh` level marker. |
| `reasoning_max` | The `max` level marker. |

Tool block backgrounds by state:

| Role | What it paints |
|---|---|
| `tool_pending_bg` | A running tool block's background. |
| `tool_success_bg` | A finished tool block's background. |
| `tool_error_bg` | A failed tool block's background. |

The `/context` grid categories (one cell color per estimated category, plus
the free space and the compaction buffer):

| Role | What it paints |
|---|---|
| `ctx_system` | The system prompt cell. |
| `ctx_files` | The context files cell. |
| `ctx_memory` | The memory cell. |
| `ctx_tools` | The built-in tools cell. |
| `ctx_skills` | The skills cell. |
| `ctx_mcp` | The MCP tools cell. |
| `ctx_user` | The user messages cell. |
| `ctx_assistant` | The assistant messages cell. |
| `ctx_reasoning` | The thinking cell. |
| `ctx_results` | The tool results cell. |
| `ctx_images` | The images cell. |
| `ctx_free` | The free space. |
| `ctx_buffer` | The compaction buffer. |

Footer:

| Role | What it paints |
|---|---|
| `model` | The model item. |
| `tier` | The tier item. |
| `usage` | The usage item. |
| `mcp` | The MCP item. |
| `budget` | The context share. |
| `budget_over` | The over-budget highlight. |
| `footer_separator` | The separator between items, weaker than the item text. |

Diff lines:

| Role | What it paints |
|---|---|
| `diff_added` | Added lines. |
| `diff_removed` | Removed lines. |
| `diff_context` | Context lines. |
| `diff_hunk` | File headers and `@@` markers. |

Rendered diagrams:

| Role | What it paints |
|---|---|
| `mermaid_node` | Diagram nodes. |
| `mermaid_edge` | Diagram edges. |
| `mermaid_text` | Diagram text. |

Assistant Markdown:

| Role | What it paints |
|---|---|
| `md_heading` | Heading text. |
| `md_heading_mark` | The `#` mark, one level weaker than the heading text. |
| `md_link` | Link text. |
| `md_link_url` | The URL. |
| `md_code` | Inline code. |
| `md_quote` | Quoted text. |
| `md_quote_border` | The quote rail. |
| `md_hr` | Horizontal rules. |
| `md_list_bullet` | List bullets. |

Code-block syntax:

| Role | What it paints |
|---|---|
| `syntax_comment` | Comments. |
| `syntax_keyword` | Keywords. |
| `syntax_function` | Function names. |
| `syntax_variable` | Variables. |
| `syntax_string` | Strings. |
| `syntax_number` | Numbers. |
| `syntax_type` | Types. |
| `syntax_operator` | Operators. |
| `syntax_punctuation` | Punctuation. |

## Light versus dark terminals

The file never names a terminal background. `appearance` decides the
starting palette: `dark` for dark terminals, `light` for light ones. Set
every role you care about and let `appearance` carry the rest. `mono` is not
an `appearance` value; it is a full built-in theme for terminals without
color.

## Importing a pi theme

A pi theme JSON converts once, by code. It never runs as the theme format.

```sh
radian theme import ~/.pi/agent/themes/kanagawa-wave.json [--name <name>] [--force]
```

It writes `~/.radian/themes/<name>.toml`: the file's own `name`, else
`--name`, else the input file stem. It prints the written path and one report
line per mapped value, naming every pi key it drops and every role that keeps
the built-in value. It refuses to overwrite an existing file without
`--force`. A missing or bad input file exits with code 1, as does an
existing output without `--force`.

What converts: pi's text levels and semantic colors map onto the matching
roles (user message text to `user`, thinking text to `reasoning`, tool title
to `tool_call`). The import report names every mapped value, every pi key it
drops, and every role that keeps the built-in value: read it for the full
mapping of your file. What does not convert: pi roles nothing consumes
yet (panel roles and several Markdown, diff and syntax families) are dropped
and reported. The result is a starting point: tune it with `[tui.colors]`
or by editing the file.

There is no command that exports a built-in theme. Start a hand-written file
from the worked example below, or import a pi theme and edit the result.

## Validation errors

Every problem warns once on the startup line and never blocks startup. The
shapes: an unknown theme name (`no theme "x": built-ins are dark, light,
mono; theme files live in …`), a broken file (uses built-in `dark`), a top
level that is not a table, a missing `[roles]` table, an unknown role
(`unknown role(s) … ignored`), a bad role value (names the role and uses the
built-in color), a missing variable, and a variable loop. `NO_COLOR` set is
not an error: the theme is fixed to `mono`.

## A complete theme from scratch

This file builds a full theme on the `dark` palette, overriding only the
roles that carry its accent. Copy it to `~/.radian/themes/harbor.toml` and
run `/theme harbor`.

```toml
name = "harbor"
appearance = "dark"

[vars]
accent = "#7aa2f7"
good = "#9ece6a"
bad = "#f7768e"
weak = "#565f89"

[roles]
# Text levels
text = "#c0caf5"
user = "$accent"
notice = "#c0caf5"
notice_label = "$accent"
notice_body = "#9aa5ce"
hint = "$weak"
truncation = "$weak"
interrupted = "#e0af68"

# Structure
frame = { fg = "$weak" }
selection = { bg = "#283457" }
completion = { fg = "#c0caf5", bg = "#283457" }
find = { bg = "#283457" }

# Thinking
reasoning = "#9aa5ce"
reasoning_high = "$accent"
reasoning_max = "$accent"

# Tools
tool_call = "#c0caf5"
tool_name = "#7aa2f7"
tool_result = "#9aa5ce"
tool_result_error = "$bad"
status_ok = "$good"
status_warn = "#e0af68"
status_error = "$bad"

# Markdown and code
md_heading = "$accent"
md_heading_mark = "$weak"
md_link = "$accent"
md_link_url = "$weak"
md_code = "#9ece6a"
md_quote = "#9aa5ce"
md_quote_border = "$weak"
md_hr = "$weak"
md_list_bullet = "$accent"
syntax_comment = "$weak"
syntax_keyword = "$accent"
syntax_string = "$good"

# Diff
diff_added = "$good"
diff_removed = "$bad"
diff_hunk = "$accent"

# Footer
model = "$accent"
tier = "#9aa5ce"
usage = "#9aa5ce"
mcp = "$accent"
budget = "#9aa5ce"
budget_over = "#e0af68"
auto = "#e0af68"
footer_separator = "$weak"
approval_border = "$accent"
turn_running = "#e0af68"
turn_idle = "$good"
turn_cancelled = "$bad"
```

Every role not named here keeps the `dark` built-in. Type `/theme harbor`
to apply it, then adjust one role at a time with `[tui.colors]` before
editing the file: the config override and the file share the value shape, so
a tuned override pastes straight into `[roles]`.

## The footer

The footer is the status line under the input box. It follows the theme
roles above and is configured separately: `footer.md` lists every preset,
row, item and command item, including the `loop` item, each item's own
parameters, the per-preset rows, and `tui.widgets`.

A `[tui.colors]` key, a theme file role, and a footer item `style` all name
one slot each in the same table the renderer uses, so the footer follows
the theme.

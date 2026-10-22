---
description: Shape the status line under the input box: presets, rows, items and command items.
---

# The footer

The footer is the status area under the input box. It is data, not code:
`[tui.footer]` in the config file picks a preset, a glyph set and rows of
items. Every key below takes effect at the next startup. A bad value warns
once on the startup line and keeps its default; nothing here blocks startup.

`radian footer export <preset>` prints one built-in preset's TOML exactly.
Pasting that output as `[tui.footer]` and editing it customizes from that
preset. An unknown preset name prints the four names on stderr and exits 2.

## `[tui.footer]`

| Key | Type | Default | Values | What it does |
|---|---|---|---|---|
| `preset` | string | `deck` | `deck`, `full`, `pi`, `classic` | Which built-in rows draw when `rows` is empty. |
| `separator` | string | `" · "` | any string | The glyphs between two items. A row may override it. |
| `separator_style` | style | `footer_separator` | a theme role, one of `dim`, `muted`, `accent`, `warn`, `error`, `ok`, `text`, or a fixed color | The color of the separator. A bad value warns and keeps `footer_separator`. |
| `glyphs` | string | `unicode` | `unicode`, `nerd`, `ascii`, `emoji` | Which icon set items draw. `nerd` needs a Nerd Font; `emoji` draws through the system emoji font. An unknown value warns and keeps `unicode`. |
| `context_line` | string | `off` | `off`, `percentage` | How the gap between a row's left and right groups draws. `percentage` fills it with a `─` rule: the used share of the context window in the `budget` role, the rest in `footer_separator`. With no known budget the gap stays spaces. An unknown value warns and keeps `off`. |
| `transparent` | boolean | `false` | `true`, `false` | `true` clears every footer cell background, so the terminal's own background shows. |
| `items` | table | none | `[tui.footer.items.<id>]` tables | Per-item defaults. A row that names the item by bare id takes them; an inline table in the row wins. |
| `rows` | array | none | `[[tui.footer.rows]]` tables | Your own rows. Empty means the preset's rows draw. |

One structural mistake anywhere in `[tui.footer]` (an unknown key, a wrong
type, a row with an unknown key, an item that is neither a string nor a
table, a threshold without an integer `at` and a string `style`) drops the
whole block with a warning, and the `deck` preset draws.

## Rows

| Field | Type | Default | Values | What it does |
|---|---|---|---|---|
| `left` | array | empty | item ids or inline item tables | Items before the gap. |
| `right` | array | empty | item ids or inline item tables | Items after the gap. |
| `separator` | string | the footer's `separator` | any string | The glyphs between this row's items. |
| `position` | string | `footer` | `footer`, `above_input`, `below_input` | Where the row draws: the status area, one line above the input box, or one line below it. An unknown value warns and draws at `footer`. |

A row with no data renders empty and takes no space. `[tui.widgets]`
is the old spelling of `position`: `above_input` and `below_input` id lists
fold into footer rows, with one deprecation warning.

## Items

An item is a bare id (`"model"`) or an inline table
(`{ item = "cwd", format = "{repo}" }`). A bare id takes
`[tui.footer.items.<id>]`; an inline table is used as written, except that
`[tui.footer.items.<id>]` still supplies the item's own parameters. A table
without an `item` key is a structural mistake and drops the whole
`[tui.footer]` block.

| Field | Type | Default | What it does |
|---|---|---|---|
| `item` | string | none | The item id. A built-in id draws its component; an id with a `command` parameter draws that command's value; any other id draws a contributor's published text once one exists, else its own id text with one warning. |
| `icon` / `symbol` | string | the glyph set's icon | Both names set the same field. Drawn before the text. `""` draws no icon. |
| `symbol_style` | style | the item's text style | The icon's own style, when it differs from the text's. A bad value warns and follows the text. |
| `format` | string | the item's own default | `{placeholder}` names from the item's section below, plus `[text](style)` spans with an inline style. An unknown placeholder or a bad inline style warns and shows the format as written. |
| `style` | style | the item's own role | A theme role, one of `dim`, `muted`, `accent`, `warn`, `error`, `ok`, `text`, or a fixed color. A bad value warns and keeps the role. A style the data itself sets (a thinking level, a stale mark) wins over it. |
| `priority` | integer | the item's own number | Higher survives a narrow terminal longer. |
| `thresholds` | array | the item's own list | `{ at = 70, style = "warn" }` entries in file order. Only `context`, `cost`, `pending` and `queued` have a number to compare, so only they ever match one. |
| `disabled` | boolean | `false` | `true` hides the item everywhere it is named. A disabled command item never runs. |
| `command` | string | none | Turns the id into a command item (only outside the built-in ids; a built-in keeps the draw). Write it in `[tui.footer.items.<id>]`: only that table starts the command. |
| `period` | integer seconds | `60` | How often a command item runs. `0` runs once at startup. Any other value clamps silently into `15`–`3600`. A non-number warns and keeps `60`. |
| `width`, `fill`, `empty` | string | `10`, `━`, `─` | The `context` item's bar. A non-number `width` reads as `10`. |
| `keys` | string | all keys | The `status` item's filter: comma-separated `status.*` key names, trimmed. |

A style value is a theme role name (every name `themes.md` lists), one of
`dim`, `muted`, `accent`, `warn`, `error`, `ok`, `text`, or a fixed color:
one of `black`, `red`, `green`, `yellow`, `blue`, `purple`, `magenta`,
`cyan`, `white`, `bright_black`, `bright_red`, `bright_green`,
`bright_yellow`, `bright_blue`, `bright_purple`, `bright_magenta`,
`bright_cyan`, `bright_white`, `#rrggbb` (exactly six hex digits), or a
`0`–`255` index; plus `bold`, `italic`, `dimmed`, `underline`. The first
color is the foreground, a second the background; a third is an error. An
empty string is the default style.

## Thresholds

`thresholds` compare one number per item. Only four items have one:
`context` compares the used percent rounded to a whole number (`0`–`100`),
`cost` compares whole dollars, `pending` and `queued` compare their count.
Every other item ignores its thresholds. The default list on `context` and
`cost` is `{ at = 70, style = "warn" }` then `{ at = 90, style = "error" }`
(`warn` is the `budget_over` role, `error` the `tool_result_error` role);
every other item defaults to an empty list. When several entries match, the
last matching entry in list order wins. A style the data itself sets wins
over every threshold.

## Items

Each section names the item's placeholders, its default `format`, its icon
in each glyph set (`unicode` / `nerd` / `ascii` / `emoji`), its default
role, its priority, and when it hides. `none` as a role means the terminal
default. An item with no data hides and its row takes no space.

### `brand`

Placeholders: `brand`. Default format: `{brand}`. Icons: none (the mark is
the value). Role: `notice_label` under `unicode` and `emoji`, or whenever
`tui.brand` is set; `text` otherwise. Priority: `92`. Never hides. The value
is `tui.brand` when set (at most 4 display columns; wider is a config
error), else the set's mark: `R` (`unicode`), `󰰠` (`nerd`), `rad`
(`ascii`), `⚡` (`emoji`).

### `host`

Placeholders: `host`. Default format: `{host}`. Icons: `@ ` / `󰌋 ` /
`host ` / `💻 `. Role: `notice_body`. Priority: `78`. The machine's
hostname, read once at startup. Hides when the hostname is unknown.

### `model`

Placeholders: `display_name`, `id`, `provider`. Default format:
`{display_name}`. Icons: none. Role: `model`. Priority: `90`. Never hides:
with no model `{display_name}` is `no model` and `{id}` and `{provider}`
are empty. `{display_name}` is the display name when set, else the model
id.

### `reasoning`

Placeholders: `level`. Default format: `{level}`. Icons: `* ` / ` ` /
`* ` / `🧠 `. Role: none (the level picks its own: `off` draws in
`reasoning_off`, `minimal` in `reasoning_minimal`, `low` in
`reasoning_low`, `medium` in `reasoning_medium`, `high` in
`reasoning_high`, `xhigh` in `reasoning_xhigh`, `max` in `reasoning_max`;
any other value draws in `reasoning_medium`). A configured `style` never
overrides it. Priority: `60`. Hides unless a thinking level is set. Levels
are `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.

### `tier`

Placeholders: `tier`. Default format: `{tier}`. Icons: none. Role: `tier`.
Priority: `75`. Hides unless the session has a tier.

### `cwd`

Placeholders: `path`, `path_home`, `basename`, `repo`, `worktree`. Default
format: `{path_home}`. Icons: none. Role: none. Priority: `80`. Hides when
the working directory is unknown. `{path}` is the full path; `{path_home}`
shortens a `$HOME` prefix to `~`; `{basename}` is the last path component;
`{repo}` is the repository root's name and falls back to `{basename}`
outside a repository; `{worktree}` is empty outside a worktree.

### `git_branch`

Placeholders: `branch`, `dirty`, `modified`, `untracked`. Default format:
`{branch}`. Icons: none / ` ` / none / none. Role: none (the contribution's
role; dim while stale). Priority: `70`. Hides with no branch data.
`{modified}` and `{untracked}` are the change counts, empty when unknown;
`{dirty}` is empty on a clean tree, else ` ?` plus the total count.

### `worktree`

Placeholders: `worktree`. Default format: `worktree {worktree}`. Icons:
none. Role: none (the contribution's role; dim while stale). Priority: `70`.
Shows only inside a worktree.

### `status`

Placeholders: `status`. Default format: `{status}`. Icons: none. Role: none
(normal text; dim while stale). Priority: `45`. Joins every `status.*`
contribution with spaces. Hides with no text. Parameters: `keys` filters to
the named keys: `keys = "build, lint"` shows only `status.build` and
`status.lint`. Without `keys` every key shows.

### `session`

Placeholders: `id`. Default format: `{id}`. Icons: `# ` / none / `# ` /
`# `. Role: none. Priority: `65`. Never hides. The value is the session id.

### `context`

Placeholders: `bar`, `percent`, `used`, `budget`, `auto`. Default format:
`{used}/{budget}`. Icons: none / `󰍛 ` / `[` / none. Role: `budget`, with the
default thresholds above. Priority: `85`. Never hides: unknown counts show
`-`. `{bar}` is `width` cells (default `10`; a non-number reads as `10`),
`fill` (default `━`) for the used part, `empty` (default `─`) for the rest.
`{percent}` has one decimal (`41.3`). `{auto}` is ` ~` while automatic
compaction is on (` auto` under `ascii`), else empty.

### `usage`

Placeholders: `value`. Default format: `{value}`. Icons: none. Role: none
(normal text; dim while stale). Priority: `50`. Shows only while the current
provider reports a usage snapshot: each window's remaining share, each
window's reset countdown, then the plan word. Hides for a provider with no
usage source, or when the stored snapshot belongs to another provider.
`/usage` prints the same snapshot in full.

### `tokens`

Placeholders: `in`, `out`, `cache`, `cache_write`, `in_arrow`,
`out_arrow`. Default format under `unicode` and `emoji`:
`↑{in} ↓{out} R{cache}`; under `nerd` and `ascii`: `^{in} v{out} R{cache}`.
`{in_arrow}` is `↑` (`unicode`, `emoji`) or `^` (`nerd`, `ascii`);
`{out_arrow}` is `↓` or `v`. Icons: none. Role: `usage`. Priority: `50`.
Never hides: a count no provider reported shows `-`, never a fabricated
zero. `{cache}` is the cache-read total; `{cache_write}` is the
cache-write total and stays out of the default format. Counts render
compact: raw below 1000, `K` below a million, `M` above.

### `speed`

Placeholders: `speed`. Default format: `{speed}`. Icons: `» ` / ` ` /
`spd ` / `🚀 `. Role: `usage`. Priority: `55`. The last finished
generation's tokens per second (`42.1 tok/s`). Hides until one finishes.

### `elapsed`

Placeholders: `elapsed`. Default format: `{elapsed}`. Icons: none / ` ` /
`t ` / `⏱ `. Role: `truncation`. Priority: `35`. Live while a generation
runs, the last one when idle. Hides with no reading, and stays empty after
a resume. Renders `16.2s`, past a minute `1m02s`, past an hour `1h02m`.

### `cost`

Placeholders: `usd`. Default format: `{usd}`. Icons: none. Role: none, with
the default thresholds above. Priority: `40`. The session's accumulated fee
(`$14.510`, three decimals so a sub-cent fee never reads as free). A total
with a missing rate carries `≥`; a total re-priced from old records carries
`~`. Hides until some call bills and the catalog has a price: never a
fabricated number.

### `pending`

Placeholders: `count`. Default format: `{count} pending`. Icons: none.
Role: `turn_running`. Priority: `70`. Shows only while a request waits.

### `queued`

Placeholders: `count`. Default format: `{count} queued`. Icons: none. Role:
`turn_running`. Priority: `70`. Shows only while a queued input (`/queue`,
or a steered `Enter`) waits for its turn.

### `loop`

Placeholders: `count`, `every`, `next`. Default format varies with the
state: `loop 3/20` while iterating, `loop every 5m · 3/20` with an `--every`
wait between turns, `loop · next in 3m` while the wait runs, `loop on` while
armed with no prompt yet, `loop paused` while paused. Icons: none. Role:
`turn_running`. Priority: `70`. Shows only while the loop is active.
`{count}` is `iterations/max_iterations`.

### `jobs`

Placeholders: `count`, `age`. Default format: `{count} · {age}`. Icons:
none. Role: none. Priority: `70`. This session's running background jobs
with the oldest one's age (`2 jobs · 18m`, `1 job · 5s`). Shows only while
jobs run. The age renders `5s` under a minute, `18m` under an hour,
`2h14m` above.

### `mcp`

Placeholders: `mcp`. Default format: `{mcp}`. Icons: none. Role: `mcp`.
Priority: `55`. Always shows: the configured and connected servers and
tools.

### `scroll`

Placeholders: `position`, `mode`. Default format: `{position} {mode}`.
Icons: none. Role: none. Priority: `20`. Never hides. `{position}` is `all`
at the bottom with nothing to scroll, else `scroll/max` (for example
`12/48`). `{mode}` is `follow` or `paused`.

### Command items

Any id no built-in claims becomes a command item when its
`[tui.footer.items.<id>]` table sets `command`. Only that table starts the
command: a `command` written inline in a row marks the id (no unknown-id
warning, `{value}` validates) but starts nothing. Placeholders: `value`.
Default format: `{value}`. Icons: none, unless the output or the item sets
one. Priority: `50` unless the item sets another.

`command` is a command line, not a shell: whitespace separates words, single
and double quotes group them, a backslash escapes the next character (inside
double quotes too). An empty command, an empty executable name, an
unterminated quote or a trailing backslash warns and never runs. `period`
sets the refresh period (see the items table). The command runs as you,
unsandboxed, on that period, with stdin as one JSON object of display-safe
facts (`model`, `cwd`, `branch`, `context_percent`, `tokens` with `input`,
`output` and `cache_read`, `cost_usd`; each `null` when unknown, never a
fabricated zero). Only `PATH`, `RADIAN_SESSION_ID` and `RADIAN_CWD` pass
through from the environment; everything else is cleared.

Its stdout's first line is the value, trimmed and cut at 256 bytes on a
UTF-8 boundary (at most 64 KiB is read, so a flooding command cannot grow
memory). A line that does not start with `{` is plain text. A line that
starts with `{` must be a JSON object with a string `text` and optional
string `style` and `symbol`: `{"text": "ok", "style": "warn", "symbol": "! "}`.
`style` names one of `normal` (also `text`), `dim` (also `dimmed`,
`muted`), `warn` (also `warning`), `ok` (also `success`), `error` (also
`err`), `accent`; absent or `null` means `normal`. A JSON `symbol` draws
verbatim before the text, so include its own trailing space; a configured
`symbol` on the item wins over it.

One run may take 5 seconds; a timeout, a spawn failure, a non-zero exit, an
empty line or a bad line keeps the last value and draws it stale (dim).
Until the first good value the item hides. A `command` under an id a
built-in claims still starts its process, but its value never shows: the
built-in keeps the draw.

## Narrow terminals, numbers, unknown text

A row that is too wide drops items until it fits: the whole `right` group
first, then `left`, lowest `priority` first, ties broken toward the group's
right end. Counts render compact: raw below 1000 (`999`), `K` below a
million (`9.5K`, `15K`), `M` above (`15M`, `1962M`); one decimal below ten
of the unit, whole numbers above. Money renders `$14.510`. Percents render
with one decimal (`41.3%`).

An unknown item id, placeholder or style shows as written with one warning
each and never blocks startup — except an id a contributor publishes, which
draws that text normally. An unknown scalar parameter on a built-in item is
kept and ignored silently; a non-scalar one (an array or a table) drops the
whole `[tui.footer]` block with a warning.

## Presets

### Preset `deck`

Three rows: the provider usage line (dropped when it has no snapshot, so it
never leaves a blank line), the identity row, then the numbers row. This is
the default.

```toml
# The `deck` preset (the default): the current provider's usage
# line (dropped when it has no snapshot, so it never leaves a blank line), the identity row,
# then the numbers row.
#
# A preset file uses the same keys a user writes for `[tui.footer]`: `radian footer export
# deck` prints this file exactly, and using it as the config's `[tui.footer]` section gives
# this footer.
[tui.footer]
preset = "deck"
separator = " · "

[[tui.footer.rows]]
left = ["usage"]

[[tui.footer.rows]]
left = [
  "model",
  { item = "cwd", format = "{repo}" },
  { item = "git_branch", format = "{branch}{dirty}" },
  "worktree",
]
right = [
  { item = "context", symbol = "", format = "{bar} {percent}%" },
  "reasoning",
  "queued",
  "loop",
]

[[tui.footer.rows]]
separator = " "
right = [
  "tokens",
  "cost",
  { item = "context", symbol = "", format = "{percent}%/{budget}" },
]
```

### Preset `full`

Two rows: the identity row with host and session, then the numbers row with
speed, elapsed and background jobs.

```toml
# The `full` preset: the identity row and the numbers row.
#
# `full` includes `jobs` (this session's running background jobs and the
# oldest one's age; empty while none runs, so it takes no space); the other presets do not.
# A preset file uses the same keys a user writes for `[tui.footer]`: `radian footer export
# full` prints this file exactly, and using it as the config's `[tui.footer]` section gives
# this footer.
[tui.footer]
preset = "full"
separator = " · "

[[tui.footer.rows]]
left = [
  "brand",
  "host",
  "model",
  "reasoning",
  { item = "cwd", format = "{repo}" },
  { item = "git_branch", format = "{branch}{dirty}" },
  "session",
]

[[tui.footer.rows]]
left = [
  { item = "tokens", format = "{in_arrow}{in} {out_arrow}{out} {cache}" },
  "speed",
  "cost",
  { item = "context", format = "{percent}%/{budget}{auto}" },
  "elapsed",
  "jobs",
]
```

### Preset `pi`

Two rows: model, directory, branch, the context bar and thinking on top;
tokens and cost below.

```toml
# The `pi` preset (the earlier two-row preset): the model/cwd/branch row with the
# context bar and reasoning, then the tokens/cost row.
#
# A preset file uses the same keys a user writes for `[tui.footer]`: `radian footer export
# pi` prints this file exactly, and using it as the config's `[tui.footer]` section gives
# this footer.
[tui.footer]
preset = "pi"
separator = " · "

[[tui.footer.rows]]
left = [
  "model",
  "cwd",
  "git_branch",
  { item = "context", symbol = "", format = "{bar} {percent}%" },
]
right = ["reasoning"]

[[tui.footer.rows]]
left = ["tokens", "cost"]
```

### Preset `classic`

No rows: the renderer draws the one status line it always drew.

```toml
# The `classic` preset: today's one status line, byte-for-byte. It is drawn by the
# renderer, so it carries no rows.
#
# A preset file uses the same keys a user writes for `[tui.footer]`: `radian footer export
# classic` prints this file exactly, and using it as the config's `[tui.footer]` section
# gives this footer.
[tui.footer]
preset = "classic"
separator = " · "
```

## Worked examples

A two-row footer from scratch. Name two items per side, turn the gap into a
context gauge, and drop the separator's own styling to plain dim:

```toml
[tui.footer]
preset = "deck"
glyphs = "unicode"
context_line = "percentage"

[[tui.footer.rows]]
left = ["model", { item = "cwd", format = "{repo}" }]
right = ["reasoning"]

[[tui.footer.rows]]
left = ["tokens", "cost"]
right = [{ item = "context", format = "{percent}%/{budget}" }]
separator = " "
```

A command item that shows the current ticket. The id is your own, so no
built-in claims it; `period = 60` refreshes it every minute; `{value}` is
its only placeholder:

```toml
[tui.footer.items.ticket]
command = "~/bin/ticket"
period = 60

[[tui.footer.rows]]
left = ["model", "ticket"]
```

When `~/bin/ticket` prints `{"text": "T-123", "style": "accent"}`,
the item shows `T-123` in the accent role. When it exits non-zero, the last
ticket stays and draws dim.

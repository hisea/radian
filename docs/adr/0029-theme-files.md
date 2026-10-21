# ADR 0029: Theme Files

Status: accepted

## Background

All Radian colors come from the `Theme` struct in `crates/radian-tui/src/theme.rs`, separated by role. `render.rs` writes no colors itself. The theme is the only color source.

## Decisions

### 1. Radian theme format is TOML

`~/.radian/themes/<name>.toml`:

```toml
name = "tokyo-night"
appearance = "dark"          # dark | light; default dark

[vars]
accent = "#7aa2f7"
bg_raised = "okhsl(229 5% 24%)"

[roles]
text = "#c0caf5"
user = "$accent"             # one color: foreground only
frame = { fg = "$accent", dim = true }
selection = { bg = "#283457" }
```

- Keys are **Radian role names** (`Role` snake_case names, same set as `[tui.colors]`). A value has the same shape as `[tui.colors]`: one color string (foreground only), or a table `{ fg, bg, bold, italic, dim, underline }`.
- Color writes: `#hex`, `oklch()`, `okhsl()`, 0-255 color numbers, `$variable`, `""` for terminal default.
- Variables define only in `[vars]` and reference as `$name`. Chained references are allowed. A circular reference falls back only for affected roles.
- `tui.theme = "<name>"` finds built-in `dark`, `light`, and `mono` first, then this directory.
- Why TOML: configuration is already TOML (ADR 0008). `[tui.colors]` value shapes reuse as-is with one parser. No pi `export` (HTML export background).

### 2. Role set keeps only used roles, plus thin-style needs

- New: `frame` (thin lines, open boxes, table lines, compaction lines, `•`/`●` marks; one level weak), `tool_name` (tool row tool name), `diff_line_number` (diff line numbers, one level weak), `status_ok`/`status_warn`/`status_error` (`✓`/`×` on tool rows and similar), `ctx_system`/`ctx_files`/`ctx_memory`/`ctx_tools`/`ctx_skills`/`ctx_mcp`/`ctx_user`/`ctx_assistant`/`ctx_reasoning`/`ctx_results`/`ctx_free`/`ctx_buffer` (`/context` grid category colors; defaults in the ADR 0035 context-grid section; a theme file can override each by the same name, missing falls back to built-in).
- Changed meaning: `user` is a **foreground** role (`❯` and user text, accent color). `diff_hunk` is one level weak. `md_heading` is accent bold. `reasoning` is not italic.
- No longer used but parsing keeps them for one version (for old theme file transition): `md_heading_mark`, `tool_pending_bg`/`tool_success_bg`/`tool_error_bg`. `input_bar` left with the removed `bar` input box: writing it in a theme file is one unknown-role warning. The pi `userMessageBg` key stays in `reserved` and never draws.
- `diff_added_bg`/`diff_removed_bg` are **not added** (diff uses no background). Footer `reasoning_off` through `reasoning_max` seven levels stay.
- Dropped pi keys (dropped in translation and listed in the report): `customMessage*`, `bashMode`, `mdCodeBlockBorder`, `borderMuted`, `toolPendingBg`/`toolSuccessBg`/`toolErrorBg`, `export`.

### 3. Tolerance

Missing roles fall back to the built-in theme (built-in `dark` or `light` per `appearance`; `dark` when `appearance` is missing). Unknown roles ignore. Each problem class warns once. A broken theme file falls back wholly to the built-in theme. None blocks startup (the `tui.*` rules in ADR 0008).

### 4. Per-role override

`[tui.colors]` layers over the selected theme. Keys are **Radian role names**; the transition period also accepts pi role names (only Radian names after one version). A value can be one color (foreground only) or a table `{ fg, bg, bold, italic, dim, underline }`. Color writes match §1. A bad format warns and ignores.

### 5. One-time translation from pi themes

`radian theme import <pi-theme.json> [--name <name>]`: maps pi keys to Radian roles per a fixed table, resolves variables and `appearance`, and writes `~/.radian/themes/<name>.toml`. Each dropped key and each role that falls back to built-in reports one line. It never overwrites an existing file unless `--force`. The table is deterministic, so code does the conversion.

Built-in skill `theme-import` (ADR 0025) teaches the model when to run this command, how to write a Radian theme from only a screenshot or palette table, and how to tune levels with `[tui.colors]`. Runtime only accepts Radian role names. Pi mapping lives only in the importer.

**Current compatibility rule**: `tui.theme = "<name>"` finds `<name>.toml` first, then `<name>.json` with the old loader, and the startup line says "run `radian theme import`". `.json` only reads existing files; it never writes new `.json` themes.

### 6. Downgrade

- A terminal with true color (`COLORTERM` is `truecolor` or `24bit`) outputs RGB directly. Otherwise it approximates to the nearest 256 color (the ADR 0027 algorithm).
- `NO_COLOR` forces `mono`, ahead of theme files and overrides.
- No 16-color level exists (design uses no backgrounds; thin lines and marks work in 256 colors).
- No pi `system` theme exists (OSC terminal query is untestable under pty), no hot reload of theme files while running, and no `light`/`dark` auto switch.

### 7. Built-in themes

`dark`, `light`, and `mono` compile into the binary, not as files. `dark` and `light` values follow pi built-in theme brightness steps: primary (answers, user messages, tool titles), secondary (tool output, notice and startup information values), weak (hotkey hints, counts, truncation marks, completion notes), accent (labels, names). Thinking uses `thinkingText`. `frame` equals the `dim` value. `tool_name` equals the `syntax_keyword` value. `diff_line_number` equals `dim`. `status_ok`, `warn`, and `error` equal `turn_idle`, `note`, and `tool_result_error` values. `user` is accent foreground. `md_heading` is accent bold. `reasoning` is not italic. `diff_hunk` equals dim. `selection` background stays intentionally one level above `userMessageBg`, or a selection over a user message block stays invisible (only constrains built-in dark and light).

## Verification

- 19 pi theme fixtures convert through `radian theme import` with results stored as golden files. Conversion is deterministic and repeatable. The report lists each dropped key.
- `.toml` loads: variable chained references, circular references affecting only related roles, missing roles falling back to built-in, broken files falling back wholly; `appearance` decides which built-in is the fallback.
- Transition: `.json` still loads with a notice; same-name `.toml` wins.
- `[tui.colors]` works with Radian names; pi names work in the transition version with a notice.
- Each new role has a default and at least one snapshot uses it (`frame` in tool blocks, `tool_name` in head rows, `diff_line_number` in diff, `status_*` at head row ends).
- 256-color approximation matches known values. `NO_COLOR` wins. Built-in dark and light snapshots pin the new values.

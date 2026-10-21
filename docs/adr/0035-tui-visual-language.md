# ADR 0035: TUI Visual Language

Status: accepted

## Decisions

### V1 Principles

- **One spacing scale**: exactly 1 blank row sits between blocks (owned by the container). Markdown block elements (paragraph, heading, list, code block, table, quote, divider) also take exactly 1 row between them, owned jointly by the Markdown renderer, never by each element. Each block keeps 1 cell of left and right pad. Only background blocks keep vertical pad. Lists indent 2 cells per level. Continuation rows align to the marker **true width** (`10. ` is 4 cells). Quote prefix is 2 cells. Table cells keep 1 cell each side.
- **One left margin**: the text column starts at column 3. Code blocks, tables, quotes, and lists all indent relative to the text column. No content hugs column 1.
- **Colors come only from roles** (ADR 0029): accent, weak level, code, link, error, success, line, background band. No render code hardcodes colors.
- **Completeness beats configurability**: default theme, default glyphs (unicode), and default input box are the acceptance bar. A new config key explains its disagreement in an ADR.
- **thin**: the transcript holds **no background bands**. Columns 1-2 are the marker column, holding marks or one thin vertical `│`. **Text always starts at column 3**. Transcript boxes are all **open boxes**. Top lines carry labels `╭─ label ───…` (run to text width, never close). Each row starts with `│ ` (in the marker column). Bottom lines read `╰──` (3 cells). Closed round boxes (`panel.rs`) serve floats only. Lines always use the `frame` role.

### V2 Markdown elements

| Element | Style |
|---|---|
| Headings | Shows no `#`. Level one is accent bold underline. Level two is accent bold. Level three and below is text bold. One blank row above and below. The `md_heading_mark` role retires (parser and mapping stay for one version) |
| In-paragraph | **Bold**, *italic*, ~~strikethrough~~. Inline code takes the code role with no backticks. Link text underlines in accent, URL sits in parens one level weak. **Bold adds no color**: bold is text color plus BOLD |
| Divider | One weak-level thin line, max width **40**, never spans the screen, one blank row above and below |
| Quote | Prefix `▏` (unicode) / `│` (nerd) / `\|` (ascii), plus 1 cell. Text is one level weak |
| Lists | Unordered `-`, ordered `1.`, markers in accent. Nesting indents 2 cells per level. Continuation rows align to marker true width. Task lists `☐`/`☑` (ascii `[ ]`/`[x]`) |
| Code blocks | **Open line box**: top line `╭─ <lang> ───…` (`╭────…` with no language), each row `│ `, bottom line `╰──`, all in the marker column in `frame` color. Code starts at column 3. Syntax highlight stays. **Never wraps** |
| Tables | Full grid lines, **one rule between every two rows**. Cells keep 1 cell pad each side and align by display width. Heads use `md_heading` style, cells keep inline styles. Starts at column 3. Over-wide columns shrink and wrap, hopeless width falls back to raw text |
| Diff | Red deletes, green adds, symbols and line numbers, no row background: see ADR 0028 diff rules |
| Math | Shows source text as-is (inline and standalone alike) |
| HTML | `<br>` breaks lines, `<hr>` draws dividers, `<blockquote>` quotes, all other tags drop shells and keep text |
| Mermaid | ADR 0028 R8 / ADR 0042 |
| Thinking | Head row, duration, single-color Markdown body, folded rows: see ADR 0028. Thinking and the block below take 1 blank row between them |

### V3 Input box style

- `tui.input_style` holds two levels: **`lines` (default)**, `box`. `bar` is invalid: a config occurrence warns once at startup and runs as default `lines`.
- **`lines`**: one horizontal line above and below, `❯ ` prompt. A running turn loads the spinner and state onto the top line. No half-cell pad exists.
- **`box`**: a half-wrapped box shaped like the code-block open box. Top border `╭────` carries no label. Left `│ ` precedes `❯ ` and input. Bottom is `╰──`. A running turn writes state (`⏺ Working · 12s · esc to interrupt`) into the top border, at the same spot and spacing as the code-block language label (`╭─ <state> ───`). No bold. Colors follow existing roles.

### V4 Visual acceptance list

The visual hand-test list lives in `crates/radian-tui/README.md`. It holds one sample Markdown covering all V2 elements (mixed Chinese and English). It holds one turn with multi-tool calls, one Q&A round, and the input box. Screenshots run at 80 and 120 columns each. Checks run item by item against this ADR. TestBackend snapshots cover the same sample (`tests/thin.rs`).

### Marker column and glyphs

`unicode`/`nerd` levels:

| Entry | Columns 1-2 | Role | Notes |
|---|---|---|---|
| User message | `❯ ` | `user` (foreground role, accent) | Text also uses `user`. No background, no half-cell pad, no `▌` |
| Assistant reply | First row `● `, rest `  ` | `frame` | `●` sits on the entry first row only, unselectable, uncopied |
| Thinking | `◼ ` folded / `◻ ` expanded (with body) | `reasoning` | Body rows use `│ `. Head row and shapes: see ADR 0028 |
| Tool calls | `• ` (one row) / `◼`/`◻` (expandable body) | `frame` | Body rows use `│ `. Shapes: see ADR 0028 |
| Q&A record | `╭─ ask ───` / `│ ` / `╰──` | `frame` | Content: see ADR 0028 |
| notice / warning / `!` commands | `* ` / `! ` / `$ ` | unchanged | |
| Compaction summary | `── context compacted (…) ──` starts at column 1 | `frame` | |

- Marker column stays 2 cells, text stays column 3. `->`/`<-` retire.
- **`ascii` level mapping**: `❯`→`>`, `●`→`o`, `◼`/`◻`→`+`/`-`, `•`→`*` (one-row small tools), `│`→`|`, `╭─`/`╰──`→`+-`/`+--`, Q&A `◉`/`○`→`(*)`/`( )`, grid used/free/buffer `◼`/`◻`/`▨`→`#`/`-`/`=`.
- **Glyph exceptions** (chosen despite missing glyphs in reference fonts): `◼`/`◻` (U+25FC/U+25FB) with Q&A `◉`/`○`, plus grid buffer `▨` (U+25A8: missing in three of six fonts, same conclusion as `◼`/`◻`). Tool failure uses **`×`** (U+00D7). Thinking expanded/folded also uses `◼`/`◻`. One-row small-tool head rows with no body still use `•`.
- **Fold state** reads only **whether body rows draw below this row**. Drawn body (including `errors`-level last preview rows, error expansion, `Ctrl-O` expansion) uses `◻`. Expandable but head-row-only (such as `collapsed`-level `· N lines`, folded thinking) uses `◼`. Successful `read`/`grep`/`glob`/`list` with one row only uses `•`. Errors or `Ctrl-O` draw body, so they use `◻` the same way.
- **Multi-line commands**: fold marks draw on the block first row only. `shell` multi-line commands (heredocs and similar) fold with the block. Unfully expanded ( `◼`, or `errors`-level `◻` drawing previews only) head rows show the command first line plus `… (+N lines)` only. Fully expanded (`expanded` level, `Ctrl-O`, errors) command rest rows draw in `│ ` body before output. Meta (`exit 0 · 0.14s · N lines ✓`) always ends the first row.

### Context grid

- **Shape**: `/context` draws a 10-row × 20-column grid in the transcript. One cell is 0.5% of the context window. It shares the surface with the old plain-text notice (never a float). Grid fills by category order: each used category first, then free, then compaction buffer last. Shares round by largest remainder to exactly 200 cells. Non-empty categories keep at least one cell. Used is `◼`, free is `◻`, buffer is `▨`. The legend sits right. It shows the model name bold, the provider, `<used> / <window> tokens` marked `estimated`, provider last reports marked `reported, last request`. Italic subtitles precede one colored-block-plus-number row per category. Free and buffer rows sit last. Details sit below. Context files list row by row. MCP/skill/memory take one block each with matching commands. Message counts with compaction counts follow. `/context all` expands all files. Tight space moves the legend below the grid first. Further shortage falls back to the plain-text version (same data, other drawing).
- **Category roles**: one theme role per category, never hardcoded colors. `ctx_system` (system prompt, muted by default), `ctx_files` (context files, gold), `ctx_memory` (memory, orange-red), `ctx_tools` (tool definitions, dim), `ctx_skills` (skills, yellow), `ctx_mcp` (MCP tools, green). `ctx_user` (user messages, accent), `ctx_assistant` (assistant text, blue), `ctx_reasoning` (reasoning, thinking color), `ctx_results` (tool results, cyan), `ctx_free` (free, frame line color), `ctx_buffer` (buffer, yellow). Legend names use text color, numbers one level weak, estimated/reported marks one level weaker still.

### Panels and lists

- **Shared panel primitive `panel.rs`** draws round-corner boxes. Titles embed in top borders. `├──┤` marks segments. Bottom rows hint keys (dim keys plus muted notes). Selectors, Q&A, approvals, login, and trust modals share it. After thin, closed round boxes serve these floats only.
- **Shared list primitive `select_list.rs`** uses cursor `❯ `. It shows two columns (main and note columns, split only when width allows). Scroll windows follow highlight. Overflow draws one-column scrollbars right (weak-level track, accent thumb). Visible rows are `max(3, min(10, viewport rows - used rows - 2))`. Typing filters. No "+N more" row exists. Weak-level position `(24/110)` sits below the scrollbar (or in the status row), match counts show while filtering. Completion menus, `/model`, `/theme`, `/thinking`, and resume selectors all use it.
- **Current-item mark**: list and completion-menu current rows mark names with ` ✓` (ascii level ` *`, `status_ok` role). Note columns still align with other rows. Over-long names truncate first, never the ` ✓`.
- **`/model` in one step**: lists models of all logged-in providers, flat. Each row holds the model name in the main column and the weak-level provider in the note column. The current model marks ` ✓`. Typing matches model names and provider names together. Sort: current provider first, rest by provider then model name. The direct `/model <provider>/<model>` form stays. Same-name models under different providers take one row each.
- Lists wrap around top to bottom.

## Verification

`tests/thin.rs` full-screen snapshots cover all thin elements. `tests/context_grid.rs` full-screen snapshots cover grid, legend, and detail positions and color roles (120/80 columns and ascii level). `tests/render.rs`, `tests/card.rs`, `tests/diff.rs`, `tests/thinking.rs`, `tests/question.rs`, and `tests/footer.rs` cover blocks and lists. `blocks::tests::md_line_count_matches_the_layout`, `markdown::line_count_matches_the_layout`, and `tests/virtual.rs` cover row-count books and windows. `crates/radian-tui/scripts/check-glyph-coverage.py` checks against the README glyph table. The hand-test list in `crates/radian-tui/README.md` applies.

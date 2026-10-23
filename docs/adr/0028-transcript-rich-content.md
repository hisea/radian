# ADR 0028: Transcript Rich Content

Status: accepted

## Decisions

### R1 Display model: entries to blocks to rows

- Each entry parses into a set of **blocks** (paragraph, heading, list, quote, code block, table, diff, key-value summary, collapsed placeholder row), then lays out rows by width (`crates/radian-tui/src/blocks.rs`). Parse results cache per entry.
- Row-count cache keys by width. **Fold state is part of the cache key.** Streaming output only reparses the last block of the entry being written. Closed blocks never recompute.
- **Block spacing is uniform from the container**: exactly one blank line sits between adjacent entries. An entry strips its own head and tail background-less pure blank rows. Background pad rows are part of the block shape and stay. Row counting and drawing share one `block_gap(prev, next)` pure function.
- **A blank row means** an empty row with `band.is_none()`.
- **The marker column is 2 cells; text starts at column 3.** Marks and glyphs follow the ADR 0035 mark table. `ToolCall` `->` and `ToolResult` `<-` are retired.
- A collapsed placeholder row marks inside the marker column. The text column never jumps on fold and unfold.

### R2 Assistant text: Markdown

- CommonMark plus tables are supported. Inline styles (bold, italic, inline code, links) map to theme roles. The theme is the only color source. Links show text and URL. No OSC 8 exists.
- Code blocks: a tagged language in the table gets **syntax highlight**. Missing language or unknown language uses uniform style. Code blocks never wrap (R9).
- Tables: header cells use `md_heading` style. **Cells keep inline styles** (inline code, bold, links per their roles). Column widths count by display width. Over-wide cells wrap inside the cell. Truly too narrow falls back to raw `|` text.
- Headings **show no `#`**: level one is accent bold underline; level two is accent bold; level three and below is text bold. One blank line sits above and below.
- **Bold adds no color**: bold is text color plus BOLD.
- Code blocks use open frames (ADR 0035).

### R3 Thinking (reasoning)

- Three modes `tui.display.reasoning = "collapsed" | "expanded" | "hidden"`, default `collapsed`. Global switch action is `toggle_reasoning`, default `Ctrl-T`, rebindable.
- **Expanded**: head row `▼ thinking · <duration>` (`reasoning` role, no italic). Body rows are `│ ` (`frame` role) plus content. Body renders Markdown. **Color is all one `reasoning` color**. Bold, italic, and underline attributes stay. Code blocks never highlight. While streaming the head row is `▼ thinking · <spinner frame>`.
- **Collapsed**: one row `▶ thinking · <duration> · N lines`. While streaming it is `▶ thinking · <spinner frame>`.
- Duration is wall clock from the first reasoning delta to entry end: the TUI records two `Instant` values in projection live data, never in the journal. Format uses ADR 0027 compact numbers. Resumed entries have no instants and show no duration.
- Glyphs: expanded and collapsed use `▼` and `▶` (U+25BC and U+25B6). For block head rows with expandable content see the ADR 0035 `◼` and `◻`.

### R4 Tool calls and output

- **One call is one block**: data still holds `ToolCall` and `ToolResult` as two entries (journal and projection unchanged). Merge happens only at render: a `ToolCall` immediately followed by its own `ToolResult` draws as one block. A missing result leaves only the head row.
- **A block is one head row plus thin-line body, no background, no padding**:
  - **Head**: `• <tool> <title> · <meta> <state>`. `•` sits in the marker column in `frame` color. The tool name uses the `tool_name` role. The title uses text color. Meta is one level weak, separated by `·`, and shows only present data (`shell`: `exit 0 · 12.4s`, plus `timeout 600s` on timeout; other tools only duration; `patch` and `write` add `+N −M`). The state symbol sits at **row end**: success `✓` (`status_ok`), failure `×` (`status_error`), running pulse frame, cancelled writes `cancelled` (one level weak, no symbol).
  - A `shell` title is `$ <command>`. Multi-line commands fold with the block (see ADR 0035). Other tools use their own head-function summaries (`read path 100-160`, `patch path`, `list dir`, and more).
  - **Body**: each row is `│ ` (marker column, `frame` color) plus content. Output text is one level weak (`tool_result`). Error body text uses `tool_result_error` with `×` in the head.
- **Built-in renderers** register at compile time (`crates/radian-tui/src/tools.rs` `renderer_for`): `shell`, `read`, `write` and `patch` (diff colors), `ask_user` (question record), memory tools (key-value summary), MCP and other tools (JSON summary, else plain text). Deleted tools (`git_diff` and similar) use the generic renderer for old entries.
- **Three fold levels**: `tui.display.tool_output = "expanded" | "errors" | "collapsed"`, default `errors`. `tui.display.tool_preview_lines` default 5. `expanded` expands all. `errors` expands all failures and shows only the last `tool_preview_lines` rows for the rest. `collapsed` still expands all failures and shows only the head row for the rest, with `· N lines` added in meta. Global switch action is `toggle_tool_output`, default `Ctrl-O`.
- **Small tools take one row**: successful `read`, `grep`, `glob`, and `list` show only the head row. `Ctrl-O` or a failing call draws the body.
- No per-item fold exists: only the global switch plus R6 "auto-expand on search hit".
- Row counting and copy: whole-block rows (head included) enter the same `line_count`, `visible_lines`, selection map, and Ctrl-F. Selection and copy take content only, never lines, marks, or row numbers.
- User `!` and `!!` (ADR 0033) use another shape: two horizontal lines sandwich the command and output, not a card.

#### Diff: `patch`, `write`

- Each row: `│ ` plus right-aligned 4-digit line number plus space plus symbol plus space plus code. `+` and context rows use **new-file** numbers; `-` rows use **old-file** numbers. Symbols `+`, `-`, and space use `diff_added`, `diff_removed`, and `diff_context`. Line numbers use `diff_line_number`.
- Code highlights by file suffix through `highlight.rs`, with the same scope-to-role table as code blocks. `+` and `-` row unhighlighted text uses text color; context rows use `diff_context`. Highlight changes only color, never text. It never wraps; over-wide truncates.
- `@@` rows and file heads (`diff --git`, `+++`, `---`) use `frame` color. No row background exists.
- Line-number source: a `patch` result carries `"hunks": [{edit,old_start,new_start,old_lines,new_lines}]` in `ToolOutput.structured`; `edit` is the 1-based edit of the call that produced the hunk (`1` for the single `old`/`new` pair). Display `@@` synthesizes from it, and the card takes each hunk's `-` and `+` text from that hunk's `edit` (the item's `old`/`new` in the call's `edits` array, or the call's `old`/`new` pair); a hunk with no `edit` or one out of range draws in call order with no `@@`. Text `@@` (old entries) numbers the same way. `write` is a whole-file write: a new file draws all `+` with `new_start = 1`. Overwriting an existing file carries `"previous_lines": N` in the structured field and draws one `-1..N` / `+1..M` hunk. A diff without start lines (old entries, MCP-given text) falls back to drawing without numbers; the symbol column stays.

### R5 Other entries

- **Compaction summary message** (`origin.kind = "compaction_summary"`): collapsed shows one row `── context compacted (rev N, T_before → T_after) ──` and can expand to show summary content. The line starts at column 1 in `frame` color. Expanded summary text never changes.
- Approval records, pending-request results, and memory additions show as one `Notice`-style row.
- **User messages**: `❯ text`. `❯` and text are both accent (`user` is the foreground role). No background, no half-cell padding, no `▌` exists. They show as plain text with no Markdown render. `@` attachments still fold as `[file: P, N lines]`.

### R6 Search and folds

Ctrl-F searches **complete content**, including folded parts. A hit in a folded block auto-expands that one entry and notes it in the status bar.

### R7 Performance gates

With 50 thousand entries of mixed content (Markdown, code, and diff): a normal frame still costs below 1 ms; P95 costs below 30 ms; the first frame after a global fold switch costs below 100 ms; streaming key response never degrades. Results go into `benchmarks/perf-baseline.md`.

### R8 Charts (mermaid)

The renderer is grok-build `mermaid.rs`; details live in **ADR 0042**. This ADR only fixes display-layer behavior:

- It renders only **closed** mermaid code blocks. Pre-close shows source.
- Render is a `(source, width, theme)` pure function. Results enter the block cache. Colors take node, edge, and text roles from ADR 0029.
- On failure (unsupported syntax, over limits, short width) it **keeps the source** with one dim reason line before it. It never shows a half-drawn graph.
- Configuration `tui.display.mermaid = true | false`, default `true`.
- The model side has no mermaid prompt, tool, or skill: the model still writes ```` ```mermaid ```` blocks, and render happens only at display.

### R9 Code blocks and character art

- **Code blocks never wrap**: over-wide parts truncate and row ends show `…`. Full content comes from select and copy.
- Width uses `unicode-width` (CJK, fullwidth, box characters). Tab expands to spaces (width 4). No Markdown processing happens inside code blocks.
- Character art outside code blocks is unreliably identified and shows as normal paragraphs.

### Syntax highlight

- It uses `syntect` (pure-Rust regex backend, no C dependency). Colors come from ADR 0029 `syntax*` roles, never from syntect builtin palettes.
- Only **closed** code blocks with an info string in the common-language table highlight. `mono` never colors. `tui.display.highlight` (default `true`) turns it off. Highlight changes only color, never text.
- Syntax data is the syntect default packaged set plus vendored TypeScript, TSX, and TOML `.sublime-syntax` files (`third_party/syntaxes/`). The merged dump pregenerates and commits as `crates/radian-tui/assets/syntaxes.packdump` (`build.rs` only checks and copies; after a syntax change regenerate with `RADIAN_REGENERATE_SYNTAXES=1 cargo build -p radian-tui` and commit). Runtime only deserializes.
- Markdown uses `pulldown-cmark`.

### Question record

- It shows as an **open box**: `╭─ ask ───…`, each row `│ `, `╰──` (marker column, `frame` color). Content follows the ADR 0024 structure.
- Option marks: single, multi, and `Other` all use `○` and `◉`; marks never bold. The `ascii` level uses `( )` and `(*)`, `->`. The bottom Ask panel is floating and still a closed round box.

## Verification

`blocks.rs` and `markdown.rs` row-count and window tests. Full-screen snapshots in `tests/card.rs`, `tests/diff.rs`, `tests/thinking.rs`, `tests/question.rs`, `tests/render.rs`, and `tests/thin.rs`. `tests/highlight.rs`. `tests/find.rs`. `tests/perf_render.rs` plus `benchmarks/perf-baseline.md`. The visual hand-test list in `crates/radian-tui/README.md`. Mermaid `tests/mermaid.rs`, see ADR 0042.

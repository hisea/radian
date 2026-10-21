# ADR 0042: Mermaid Box-Line Graph Renderer (grok-build `mermaid.rs`)

Status: accepted

## Decisions

1. **The renderer is grok-build `mermaid.rs`**, vendored to `third_party/grok-mermaid/` (crate `grok-mermaid`, `src/mermaid.rs` from upstream commit `4827113`). It is the only vendored renderer in the repo.
2. **Vendored files take two recorded change kinds, all other bytes match**:
   - Two `use ratatui::…` rows become `use crate::text::{Line, Modifier, Span, Style};`. `text.rs` is a radian-written shim. `Style` is a newtype for "role" (`Style::default()` means no role). Shapes are `Span { text, style }` and `Line { spans }`. It implements only methods `mermaid.rs` uses. So the crate never depends on ratatui.
   - `pub(crate)` → `pub` (mechanical replace), so crate `lib.rs` reuses `parse_*`/`layout_*`/`render_*`.
   - Each change lists in `third_party/grok-mermaid/RADIAN-CHANGES.md`. Upstream `LICENSE` ships with `THIRD-PARTY-NOTICES`. Upgrades recopy and review diffs.
3. **The public entry** is crate `lib.rs`. `render(source: &str, max_width: Option<usize>) -> Result<Art, Reason>` . Signature takes `source` text with optional `max_width`, and returns `Result<Art, Reason>`. It copies upstream `render` dispatch ( `parse_graph` → `layout_flowchart`/`render_grouped`, `parse_state`, `parse_class`, `parse_er`, `parse_sequence`). It **never takes upstream `fallback()`**: `Reason::{Unsupported, TooWide { needed }, TooLarge}`. `needed` renders once more with `max_width = None` and takes the widest row. Upstream "open the image" prose never appears in radian. `Art` carries `lines` (each `(Option<Role>, String)`), `plain`, `width`, `height`. `Role` is `Node`/`Edge`/`Text`.
4. **TUI behavior**: success draws graphs into the block cache. Roles map `border` → `mermaid_node`. `node_text` with `edge_label`/`Text` maps to `mermaid_text`. `edge`/`Edge` maps to `mermaid_edge`. Three ADR 0029 roles apply, no new roles. Failure shows source in ADR 0035 code-block open boxes. One dim `mermaid: <cause>` row sits above. Short width writes "needs N columns, have W". Only **closed** blocks draw, `tui.display.mermaid` (default `true`) stays unchanged. TUI `blocks/markdown.rs` depends on the vendored crate `render` directly.
5. **No squeeze, no reorient**: short width falls back to source, never re-lays LR as TD.
6. **Supported kinds and caps**: flowchart (subgraph, three shapes, solid/dashed/bold lines, five edge heads, label auto-wrap), sequence, state, class, ER. Caps are 128 nodes / 512 edges / 24 groups / depth 6 / canvas-cell cap (`MAX_CANVAS_CELLS`). Short width never squeezes, falls back to source. Past 128 nodes or 512 edges upstream **silently drops** the overflow (`add_node` returns `None`). This behavior is accepted.
7. **The model side owns no mermaid tool or skill**: the built-in skill mechanism (ADR 0025) stays unchanged, `theme-import` is the only built-in skill. Models still write ```` ```mermaid ```` blocks, render lives wholly at display.
8. **MSRV follows workspace (1.99.1)**. The vendored crate uses edition 2024 (workspace already uses 2024).

## Acceptance

- `cargo tree -p grok-mermaid -e normal` shows only `unicode-width`. No second vendored renderer crate lives in the repo.
- `third_party/grok-mermaid/src/mermaid.rs` diffs against upstream `4827113` only in `use` rows and `pub(crate)` → `pub` (check script or test: restoring both replaces matches sha256 against recorded values). All upstream embedded tests pass.
- Snapshots: research-doc samples pin output at 80 and 120 columns (`radian-tui/tests/mermaid.rs`). Over-wide blocks show source plus one `mermaid: needs N columns, have W` row. Subgraph with 4-participant sequence charts draw at 80 columns.
- All Verify commands run.

## Mutations

- Role mapping off by one (`edge` uses `mermaid_text`) turns color snapshots red.
- Short width taking upstream `fallback()` ("open the image" appears) turns TUI snapshots red.
- `needed` uncomputed (writes 0) turns cause-row tests red.
- One more upstream row changed outside `use` rows turns fidelity checks red.

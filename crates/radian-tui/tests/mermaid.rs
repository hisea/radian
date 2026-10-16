//! The transcript's Mermaid blocks (ADR 0042): a closed ```mermaid block is drawn as a
//! Unicode diagram when `tui.display.mermaid` is on, and falls back to the source (with a dim
//! reason line) whenever the renderer cannot draw it. `TestBackend` frames, so the three
//! diagram roles' colors are asserted on real cells.
//!
//! The lower half pins the eight research samples (ADR 0042's acceptance): six render at 80
//! columns, two are `TooWide`, and `long labels` fits at 120. The expected bytes are the
//! `grok-build mermaid.rs` section of `docs/research/2026-10-16-mermaid-renderer-candidates/`
//! `outputs.txt` — the output of the very file vendored under `third_party/grok-mermaid/`.

use radian_tui::app::{App, EntryKind};

/// Puts a test on the pre-ADR one-line status bar (`preset = classic`; ADR 0027 C2). The
/// default footer is the two-row `pi` layout now; a geometry test keeps the layout it was
/// written for.
fn classic_footer(app: &mut App) {
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    });
}
use radian_tui::blocks::{self, ReasoningMode};
use radian_tui::render;
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A minimal flowchart the renderer draws well inside 80 columns.
const FLOW: &str = "flowchart LR; A[Start] -- go --> B[End]";

/// The modes the tests lay out with: both folds expanded, diagrams on.
fn modes(reasoning: ReasoningMode, mermaid: bool) -> DisplayModes {
    DisplayModes {
        reasoning,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid,
        highlight: true,
        glyphs: radian_tui::footer::Glyphs::Unicode,
    }
}

fn assistant(app: &mut App, text: &str) {
    app.session
        .entries
        .push(radian_tui::app::Entry::new(EntryKind::Assistant, text));
}

fn fenced(source: &str, closed: bool) -> String {
    let mut text = format!("```mermaid\n{source}\n");
    if closed {
        text.push_str("```\n");
    }
    text
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

/// The style of the first cell whose symbol equals `needle`.
///
/// The index is the cell's, not a byte offset in the row string: the diagram's box-drawing
/// characters are three bytes each, so `str::find` would land on the wrong column.
fn style_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<ratatui::style::Style> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buffer.cell((x, y))
                && cell.symbol() == needle
            {
                return Some(cell.style());
            }
        }
    }
    None
}

/// A closed flowchart on a wide screen draws box drawing and shows its frame (ADR 0035
/// revision 6 decision 7: the block's top line is titled with the language).
#[test]
fn a_closed_flowchart_is_drawn_and_the_frame_is_shown() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, true));
    let text = screen(&render(&mut app, 80, 20));
    assert!(
        text.contains('┌') || text.contains('└') || text.contains('│'),
        "the diagram's box drawing is on screen:\n{text}"
    );
    assert!(text.contains("Start"), "a node label is drawn:\n{text}");
    assert!(
        text.contains("╭─ mermaid"),
        "the frame's top line titles the block:\n{text}"
    );
    assert!(
        !text.contains("flowchart LR"),
        "the source does not show when the diagram renders:\n{text}"
    );
}

/// The same diagram at a width it cannot fit falls back to the source plus the dim reason line,
/// which names the columns the drawing needs (ADR 0042 decision 4).
#[test]
fn a_diagram_that_does_not_fit_keeps_the_source_and_names_the_reason() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, true));
    // Narrow enough that the renderer reports `TooWide`.
    let text = screen(&render(&mut app, 16, 20));
    assert!(
        text.contains("mermaid: needs"),
        "the reason line names the needed columns:\n{text}"
    );
    assert!(
        text.contains("flowchart") || text.contains("A[Start]"),
        "the source is kept:\n{text}"
    );
    assert!(
        !text.contains('┌') && !text.contains('└'),
        "no half diagram is drawn:\n{text}"
    );
}

/// An unclosed block (still streaming) shows the source; it renders only once closed.
#[test]
fn an_unclosed_block_shows_the_source() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, false));
    let text = screen(&render(&mut app, 80, 20));
    assert!(text.contains("flowchart LR"), "the source shows:\n{text}");
    assert!(
        !text.contains('┌') && !text.contains('└'),
        "nothing is drawn before the fence closes:\n{text}"
    );
}

/// `tui.display.mermaid = false` renders every code block as source.
#[test]
fn the_config_flag_disables_the_diagram() {
    let mut app = App::new("m", "s");
    app.mermaid_enabled = false;
    assistant(&mut app, &fenced(FLOW, true));
    let text = screen(&render(&mut app, 80, 20));
    assert!(text.contains("flowchart LR"), "the source shows:\n{text}");
    assert!(
        !text.contains('┌') && !text.contains('└'),
        "nothing is drawn when the flag is off:\n{text}"
    );
}

/// The renderer's roles map onto the theme's three mermaid roles (ADR 0042 decision 4): box
/// drawing is the node role, a connector and its head the edge role, and a label (node or edge)
/// the text role. `┌` is diagram-only (the code frame uses `╭`/`╰`), and `▶` is the arrowhead.
#[test]
fn the_diagram_roles_come_from_the_theme() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    assistant(&mut app, &fenced(FLOW, true));
    let terminal = render(&mut app, 80, 20);
    let node = style_of(&terminal, "┌").expect("a box-drawing char");
    let edge = style_of(&terminal, "▶").expect("an arrowhead");
    // The first `S` on screen is the node label `Start` (the frame is `╭─ mermaid`).
    let text = style_of(&terminal, "S").expect("a node label");
    assert_eq!(node.fg, app.theme.mermaid_node.fg, "box drawing is a node");
    assert_eq!(
        edge.fg, app.theme.mermaid_edge.fg,
        "the connector is an edge"
    );
    assert_eq!(
        text.fg, app.theme.mermaid_text.fg,
        "the label is plain text"
    );
    assert_ne!(node.fg, edge.fg, "nodes and edges differ");
}

/// A width change re-renders: the counted lines equal the laid-out (drawn) body lines at both
/// widths, and the two widths lay out differently.
#[test]
fn a_width_change_recounts_and_redraws() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    assistant(&mut app, &fenced(FLOW, true));
    let entry = &app.session.entries[0];
    let blocks = blocks::parse_blocks(entry.text.as_str());
    for width in [80usize, 16] {
        let modes = modes(ReasoningMode::Expanded, true);
        let count = blocks::line_count(entry, modes, width);
        let laid_out = blocks::layout(entry, &blocks, modes, width).len();
        assert_eq!(count, laid_out, "width {width}: count == laid out");
    }
    // Wide: the diagram renders (box drawing).
    let wide = screen(&render(&mut app, 80, 20));
    assert!(wide.contains('┌'), "wide draws the diagram:\n{wide}");
    // The **immutable** layout path (selection, search, the reference renderer) reads the
    // cache; it must not reuse the 80-column diagram at 16 (the width is in the key). This
    // runs before any 16-column mutable pass, so the cache still holds the wide render.
    let laid_out_16 = blocks::layout(
        &app.session.entries[0],
        &blocks,
        modes(ReasoningMode::Expanded, true),
        16,
    );
    assert!(
        laid_out_16.iter().any(|line| line.reason),
        "a 16-column layout after an 80-column render is the fallback"
    );
    let narrow = screen(&render(&mut app, 16, 20));
    assert!(
        narrow.contains("mermaid: needs"),
        "narrow names the reason:\n{narrow}"
    );
    assert!(!narrow.contains('┌'), "narrow draws no diagram:\n{narrow}");
    // The **cached** count follows the width too: after the 80-column render filled the
    // diagram cache, the 16-column count must be the narrow layout's, or virtualization
    // would count rows the renderer does not draw.
    let entry = &mut app.session.entries[0];
    let cached_16 = blocks::line_count_cached(entry, modes(ReasoningMode::Expanded, true), 16);
    let laid_out_16 =
        blocks::layout(entry, &blocks, modes(ReasoningMode::Expanded, true), 16).len();
    assert_eq!(
        cached_16, laid_out_16,
        "the cached count must be re-measured at the new width"
    );
}

const SAMPLES: &[(&str, &str)] = &[
    (
        "chain-6 TD",
        "flowchart TD\n  A[Start] --> B[Parse config]\n  B --> C[Open session]\n  C --> D[Run turn]\n  D --> E[Write journal]\n  E --> F[Exit]",
    ),
    (
        "diamond fork/merge",
        "flowchart TD\n  A[User input] --> B{Tool call?}\n  B -- yes --> C[Run tool]\n  B -- no --> D[Reply]\n  C --> E[Append result]\n  E --> A\n  D --> F[Done]",
    ),
    (
        "LR chain",
        "flowchart LR\n  TUI --> Host --> Core --> Provider",
    ),
    (
        "radian crates (12 nodes)",
        "flowchart TD\n  cli[agent-cli] --> host[radian-host]\n  tui[radian-tui] --> host\n  host --> core[agent-core]\n  host --> sess[radian-session]\n  host --> rt[radian-runtime]\n  host --> perm[radian-permissions]\n  host --> sb[radian-sandbox]\n  host --> coding[radian-coding]\n  host --> mem[radian-memory]\n  host --> ext[agent-extensions]\n  ext --> core\n  coding --> sb",
    ),
    (
        "sequence",
        "sequenceDiagram\n  participant U as User\n  participant T as TUI\n  participant H as Host\n  participant M as Model\n  U->>T: prompt\n  T->>H: turn\n  H->>M: request\n  M-->>H: tool_use\n  H->>H: run tool (sandbox)\n  H->>M: tool_result\n  M-->>H: text\n  H-->>T: events\n  T-->>U: render",
    ),
    (
        "state",
        "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Streaming: prompt\n  Streaming --> Tool: tool_use\n  Tool --> Streaming: result\n  Streaming --> Idle: end_turn\n  Idle --> [*]",
    ),
    (
        "subgraph",
        "flowchart TD\n  subgraph Host\n    A --> B\n  end\n  B --> C",
    ),
    (
        "long labels",
        "flowchart LR\n  A[Read AGENTS.md and the ADRs that touch the change] --> B[Write the failing test first] --> C[Implement until the Verify commands pass]",
    ),
];

/// The six that render at 80 columns: the `grok-build mermaid.rs · width 80` section of
/// the research `outputs.txt`, byte for byte.
const RENDERED_80: &[(&str, &[&str])] = &[
    (
        "chain-6 TD",
        &[
            "     ┌───────┐",
            "     │ Start │",
            "     └───┬───┘",
            "         │",
            "         ▼",
            " ┌──────────────┐",
            " │ Parse config │",
            " └───────┬──────┘",
            "         │",
            "         ▼",
            " ┌──────────────┐",
            " │ Open session │",
            " └───────┬──────┘",
            "         │",
            "         ▼",
            "   ┌──────────┐",
            "   │ Run turn │",
            "   └─────┬────┘",
            "         │",
            "         ▼",
            " ┌───────────────┐",
            " │ Write journal │",
            " └───────┬───────┘",
            "         │",
            "         ▼",
            "     ┌──────┐",
            "     │ Exit │",
            "     └──────┘",
        ],
    ),
    (
        "diamond fork/merge",
        &[
            "         ┌────────────┐",
            "         │ User input │◄──────┐",
            "         └──────┬─────┘       │",
            "                │             │",
            "                ▼             │",
            "         ╭────────────╮       │",
            "         │ Tool call? │       │",
            "         ╰──────┬─────╯       │",
            "         ┌──────┴───────┐     │",
            "         ▼yes           ▼no   │",
            "   ┌──────────┐     ┌───────┐ │",
            "   │ Run tool │     │ Reply │ │",
            "   └─────┬────┘     └───┬───┘ │",
            "         │              │     │",
            "         ▼              ▼     │",
            " ┌───────────────┐  ┌──────┐  │",
            " │ Append result ├──│ Done │──┘",
            " └───────────────┘  └──────┘",
        ],
    ),
    (
        "LR chain",
        &[
            "",
            "┌─────┐    ┌──────┐    ┌──────┐    ┌──────────┐",
            "│ TUI ├───▶│ Host ├───▶│ Core ├───▶│ Provider │",
            "└─────┘    └──────┘    └──────┘    └──────────┘",
        ],
    ),
    (
        "sequence",
        &[
            "┌──────┐  ┌─────┐ ┌──────┐                 ┌───────┐",
            "│ User │  │ TUI │ │ Host │                 │ Model │",
            "└───┬──┘  └──┬──┘ └───┬──┘                 └───┬───┘",
            "    │        │        │                        │",
            "    │ prompt │        │                        │",
            "    ├───────▶│        │                        │",
            "    │        │        │                        │",
            "    │        │  turn  │                        │",
            "    │        ├───────▶│                        │",
            "    │        │        │                        │",
            "    │        │        │        request         │",
            "    │        │        ├───────────────────────▶│",
            "    │        │        │                        │",
            "    │        │        │        tool_use        │",
            "    │        │        │◄╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤",
            "    │        │        │                        │",
            "    │        │        ├──╮                     │",
            "    │        │        │  │ run tool (sandbox)  │",
            "    │        │        │◄─╯                     │",
            "    │        │        │                        │",
            "    │        │        │      tool_result       │",
            "    │        │        ├───────────────────────▶│",
            "    │        │        │                        │",
            "    │        │        │          text          │",
            "    │        │        │◄╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤",
            "    │        │        │                        │",
            "    │        │ events │                        │",
            "    │        │◄╌╌╌╌╌╌╌┤                        │",
            "    │        │        │                        │",
            "    │ render │        │                        │",
            "    │◄╌╌╌╌╌╌╌┤        │                        │",
            "    │        │        │                        │",
            "┌───┴──┐  ┌──┴──┐ ┌───┴──┐                 ┌───┴───┐",
            "│ User │  │ TUI │ │ Host │                 │ Model │",
            "└──────┘  └─────┘ └──────┘                 └───────┘",
        ],
    ),
    (
        "state",
        &[
            "           ╭───╮",
            "           │ ● │",
            "           ╰─┬─╯",
            "             │",
            "             ▼",
            "         ╭──────╮      end_turn",
            "         │ Idle │◄──────────────┐",
            "         ╰───┬──╯               │",
            "       ┌─────┴─────┐            │",
            "       ▼prompt     ▼            │",
            " ╭───────────╮   ╭───╮    result│",
            " │ Streaming ├◄──│ ● │──────────┴┐",
            " ╰─────┬─────╯   ╰───╯           │",
            "       │                         │",
            "       ▼tool_use                 │",
            "   ╭──────╮                      │",
            "   │ Tool ├──────────────────────┘",
            "   ╰──────╯",
        ],
    ),
    (
        "subgraph",
        &[
            "┌ Host ┐",
            "│ ┌───┐│",
            "│ │ A ││",
            "│ └─┬─┘│",
            "│   │  │",
            "│   ▼  │",
            "│ ┌───┐│",
            "│ │ B ││",
            "│ └───┘│",
            "└───┬──┘",
            "    │",
            "    ▼",
            "  ┌───┐",
            "  │ C │",
            "  └───┘",
        ],
    ),
];

/// `long labels` at 120 columns (it needs 84): the `width 120` section of `outputs.txt`.
const RENDERED_120_LONG_LABELS: &[&str] = &[
    "",
    "┌────────────────────────┐    ┌────────────────────────┐    ┌──────────────────────┐",
    "│ Read AGENTS.md and the │    │ Write the failing test │    │ Implement until the  │",
    "│  ADRs that touch the   ├───▶│         first          ├───▶│ Verify commands pass │",
    "│         change         │    └────────────────────────┘    └──────────────────────┘",
    "└────────────────────────┘",
];

// ---------------------------------------------------------------------------
// The eight research samples (ADR 0042 acceptance).
// ---------------------------------------------------------------------------

fn sample(name: &str) -> &'static str {
    SAMPLES
        .iter()
        .find(|(sample_name, _)| *sample_name == name)
        .map(|(_, source)| *source)
        .unwrap_or_else(|| panic!("no sample named `{name}`"))
}

/// The six samples that render at 80 columns match the research output byte for byte
/// (the vendored file's own output: `grok-build mermaid.rs · width 80`).
#[test]
fn the_six_samples_render_byte_for_byte_at_80_columns() {
    for (name, expected) in RENDERED_80 {
        let art = grok_mermaid::render(sample(name), Some(80))
            .unwrap_or_else(|reason| panic!("`{name}` must render at 80: {reason:?}"));
        assert_eq!(art.plain, *expected, "sample `{name}` at 80 columns");
        assert_eq!(art.height, expected.len(), "sample `{name}` height");
    }
}

/// `long labels` is `TooWide` at 80 and renders at 120; at 120 it matches the research output.
#[test]
fn the_long_labels_sample_renders_at_120_columns() {
    let art = grok_mermaid::render(sample("long labels"), Some(120)).expect("fits at 120");
    assert_eq!(art.plain, RENDERED_120_LONG_LABELS);
    assert_eq!(art.width, 84);
}

/// The two samples that do not fit at 80 report the columns they need (ADR 0042 decision 3):
/// the 12-node graph needs 129 at either width, and the long labels need 84.
#[test]
fn the_two_wide_samples_report_the_columns_they_need() {
    use grok_mermaid::Reason::TooWide;
    assert_eq!(
        grok_mermaid::render(sample("radian crates (12 nodes)"), Some(80)),
        Err(TooWide { needed: 129 })
    );
    assert_eq!(
        grok_mermaid::render(sample("radian crates (12 nodes)"), Some(120)),
        Err(TooWide { needed: 129 })
    );
    assert_eq!(
        grok_mermaid::render(sample("long labels"), Some(80)),
        Err(TooWide { needed: 84 })
    );
    assert!(grok_mermaid::render(sample("long labels"), Some(120)).is_ok());
}

/// The transcript shows the 12-node sample as its source plus `mermaid: needs 129 columns,
/// have 80` (ADR 0042 decision 4) — the exact reason-line text.
#[test]
fn a_too_wide_sample_names_the_columns_in_the_transcript() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(sample("radian crates (12 nodes)"), true));
    let entry = &app.session.entries[0];
    let blocks = blocks::parse_blocks(entry.text.as_str());
    let text = blocks::layout(entry, &blocks, modes(ReasoningMode::Expanded, true), 80)
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("mermaid: needs 129 columns, have 80"),
        "the reason line:\n{text}"
    );
    assert!(text.contains("flowchart TD"), "the source is kept:\n{text}");
    // And on screen.
    let screen = screen(&render(&mut app, 80, 40));
    assert!(
        screen.contains("needs 129 columns, have 80"),
        "the reason line on screen:\n{screen}"
    );
}

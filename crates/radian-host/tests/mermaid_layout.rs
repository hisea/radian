//! ADR 0028: Radian's own layered flowchart layout, and the fallback to the vendored
//! renderer for everything the layout cannot draw.
//!
//! A snapshot here is plain text: [`render`] is a pure function of `(source, width)`, so the
//! rows checked into this file are the repository's record of the drawing (no `insta`). The
//! ADR's acceptance asks for the owner's six-node `flowchart TD` (the `diagrams` skill's own
//! example — the source of the screenshot the revision was written from), a diamond, one back
//! edge, an `LR` chain, and a diagram outside the subset that falls back byte for byte.

use codex_mermaid::render as vendored;
use radian_host::mermaid::{render, render_spans};

/// The `diagrams` skill's six-node `flowchart TD`: a chain, a decision, a labelled fork and a
/// bend.
const CHAIN: &str = "flowchart TD\n    A[User input] --> B[Composer]\n    B --> C{Command?}\n    C -->|yes| D[Slash command]\n    C -->|no| E[Inbox]\n    E --> F[Driver turn]\n";
/// A diamond: one split, then one join.
const DIAMOND: &str =
    "flowchart TD\n    A[Start] --> B[Left]\n    A --> C[Right]\n    B --> D[Join]\n    C --> D\n";
/// A back edge: `Fix` points at the layer above it.
const BACK: &str = "flowchart TD\n    A[Read] --> B{Valid?}\n    B -->|no| C[Fix]\n    C --> B\n";
/// An `LR` chain: the same drawing turned on its side.
const LR_CHAIN: &str = "flowchart LR\n    A[Input] --> B[Parse] --> C[Draw]\n";
/// Wider than 80 columns and narrower than 120: the width decides between the vendor's reason
/// and this layout's drawing.
const FIVE_LONG: &str = "flowchart LR\n    A[aaaaaaaaaaaaaa] --> B[bbbbbbbbbbbbbb] --> C[cccccccccccccc] --> D[dddddddddddddd] --> E[eeeeeeeeeeeeee]\n";
const SELF_LOOP: &str = "flowchart TD\n    A[one] --> A\n";
const SEQUENCE: &str = "sequenceDiagram\n    participant A\n    participant B\n    A->>B: hi\n";

/// Renders and compares the rows, printing the whole drawing when they differ.
fn expect(source: &str, width: usize, golden: &[&str]) {
    let text = render(source, width).expect("this layout draws the diagram");
    assert_eq!(text.lines().collect::<Vec<_>>(), golden, "\n{text}");
}

fn rows(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

/// The display column `needle` starts at (every character drawn here is one cell wide).
fn column_of(row: &str, needle: &str) -> usize {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("no {needle:?} in {row:?}"));
    row[..byte].chars().count()
}

/// One box read off the picture: the rows of its top and bottom borders, and its columns.
#[derive(Debug, Clone, Copy)]
struct Border {
    top: usize,
    bottom: usize,
    left: usize,
    right: usize,
}

impl Border {
    fn width(&self) -> usize {
        self.right - self.left + 1
    }

    /// The column the layout connects a straight edge to: the middle of the box.
    fn centre(&self) -> usize {
        self.left + self.width() / 2
    }
}

/// The box drawn around `label`: its row, and the borders that must bracket it.
fn border_of(rows: &[String], label: &str) -> Border {
    let row = rows
        .iter()
        .position(|line| line.contains(label))
        .unwrap_or_else(|| panic!("no row carries {label:?} in\n{}", rows.join("\n")));
    // A box is `│ label │` between two borders, so the label starts two cells in.
    let left = column_of(&rows[row], label) - 2;
    let right = left + label.chars().count() + 3;
    let border = Border {
        top: row - 1,
        bottom: row + 1,
        left,
        right,
    };
    let top: Vec<char> = rows[border.top].chars().collect();
    let middle: Vec<char> = rows[row].chars().collect();
    let bottom: Vec<char> = rows[border.bottom].chars().collect();
    assert_eq!(top[border.left], '┌', "{label:?} starts its box");
    assert_eq!(top[border.right], '┐', "{label:?} ends its box");
    // An edge that leaves or enters this box's side marks the border `├`/`┤` (the vendor's
    // convention, kept here), so the label row's borders are not always plain.
    assert!(
        matches!(middle[border.left], '│' | '├' | '┤'),
        "{label:?} has a left border: {:?}",
        middle[border.left]
    );
    assert!(
        matches!(middle[border.right], '│' | '├' | '┤'),
        "{label:?} has a right border: {:?}",
        middle[border.right]
    );
    assert_eq!(bottom[border.left], '└', "{label:?} closes its box");
    assert_eq!(bottom[border.right], '┘', "{label:?} closes its box");
    border
}

/// The layer at one row of the picture, as the columns of its first and last box.
fn layer_of(rows: &[String], row: usize) -> (usize, usize) {
    let line: Vec<char> = rows[row].chars().collect();
    let left = line
        .iter()
        .position(|ch| *ch == '┌')
        .unwrap_or_else(|| panic!("no layer on {row}: {:?}", rows[row]));
    let right = line
        .iter()
        .rposition(|ch| *ch == '┐')
        .expect("a layer ends with a top-right corner");
    (left, right)
}

/// The `row`-th character of a rendered row, padded: a shorter row is blank to its right.
fn at(rows: &[String], row: usize, column: usize) -> char {
    rows[row].chars().nth(column).unwrap_or(' ')
}

/// ADR 0028: one-row-high boxes, layers centred, and a straight vertical
/// line with an arrowhead between the nodes of two adjacent layers.
#[test]
fn the_owner_chain_is_a_centred_column_of_three_row_boxes() {
    const GOLDEN: &[&str] = &[
        "       ┌────────────┐",
        "       │ User input │",
        "       └──────┬─────┘",
        "              │",
        "              │",
        "              ▼",
        "        ┌─────┴────┐",
        "        │ Composer │",
        "        └─────┬────┘",
        "              │",
        "              │",
        "              ▼",
        "       ┌──────┴─────┐",
        "       │ ◇ Command? │",
        "       └──────┬─────┘",
        "        ┌─────┴─────────┐",
        "        │ yes           │ no",
        "        ▼               ▼",
        "┌───────┴───────┐   ┌───┴───┐",
        "│ Slash command │   │ Inbox │",
        "└───────────────┘   └───┬───┘",
        "              ┌─────────┘",
        "              │",
        "              ▼",
        "       ┌──────┴──────┐",
        "       │ Driver turn │",
        "       └─────────────┘",
    ];
    expect(CHAIN, 80, GOLDEN);
    // The drawing is a function of the graph, not of the spare width: 120 columns give the
    // same picture.
    assert_eq!(
        render(CHAIN, 120).expect("this layout draws the diagram"),
        GOLDEN.join("\n"),
    );
    let rows = rows(&render(CHAIN, 80).expect("this layout draws the diagram"));
    let canvas = rows.iter().map(|row| row.chars().count()).max().unwrap();
    let chain: Vec<Border> = [
        "User input",
        "Composer",
        "◇ Command?",
        "Slash command",
        "Inbox",
        "Driver turn",
    ]
    .iter()
    .map(|label| border_of(&rows, label))
    .collect();
    for (index, border) in chain.iter().enumerate() {
        assert_eq!(
            border.bottom - border.top,
            2,
            "box {index} is three rows high"
        );
        if index > 0 {
            let previous = chain[index - 1];
            assert!(
                border.top >= previous.top,
                "box {index} is not above the one before it"
            );
            assert!(
                border.top == previous.top || border.top > previous.bottom,
                "box {index} stands beside the one before it, or a layer below"
            );
        }
    }
    // Every layer is centred in the canvas (the widest layer sets the canvas).
    for index in [0, 3] {
        let (left, right) = layer_of(&rows, chain[index].top);
        assert_eq!(
            left,
            (canvas - (right - left + 1)) / 2,
            "layer {index} is centred"
        );
    }
    // A straight edge between two adjacent layers: one vertical line in one column, the
    // arrowhead on its last cell just above the target's border.
    for (from, to) in [("User input", "Composer"), ("Composer", "◇ Command?")] {
        let from = border_of(&rows, from);
        let to = border_of(&rows, to);
        assert_eq!(to.centre(), from.centre(), "{to:?} is under {from:?}");
        let line: Vec<char> = (from.bottom + 1..to.top)
            .map(|row| at(&rows, row, from.centre()))
            .collect();
        assert_eq!(line, vec!['│', '│', '▼'], "the line into {to:?}");
    }
    // The fork: one bus row joining the three centres, and each label beside its own line.
    let command = border_of(&rows, "◇ Command?");
    let slash = border_of(&rows, "Slash command");
    let inbox = border_of(&rows, "Inbox");
    let bus = command.bottom + 1;
    assert_eq!(at(&rows, bus, slash.centre()), '┌');
    assert_eq!(at(&rows, bus, command.centre()), '┴');
    assert_eq!(at(&rows, bus, inbox.centre()), '┐');
    let labels = command.bottom + 2;
    assert_eq!(at(&rows, labels, slash.centre()), '│');
    assert_eq!(at(&rows, labels, inbox.centre()), '│');
    assert_eq!(column_of(&rows[labels], "yes"), slash.centre() + 2);
    assert_eq!(column_of(&rows[labels], "no"), inbox.centre() + 2);
    // The bend: `Inbox` is not under `Driver turn`, so the edge uses the bus row with a corner
    // under each box.
    let driver = border_of(&rows, "Driver turn");
    let bend = inbox.bottom + 1;
    assert_eq!(at(&rows, bend, driver.centre()), '┌');
    assert_eq!(at(&rows, bend, inbox.centre()), '┘');
    assert!(
        (driver.centre() + 1..inbox.centre()).all(|column| at(&rows, bend, column) == '─'),
        "the bend's bus is one line"
    );
    let line: Vec<char> = (bend + 1..driver.top)
        .map(|row| at(&rows, row, driver.centre()))
        .collect();
    assert_eq!(line, vec!['│', '▼'], "the line into {driver:?}");
}

/// The same fork and join rules on a diamond, drawn and checked in one go.
#[test]
fn a_diamond_splits_and_joins_on_the_bus_rows() {
    const GOLDEN: &[&str] = &[
        "     ┌───────┐",
        "     │ Start │",
        "     └───┬───┘",
        "    ┌────┴─────┐",
        "    │          │",
        "    ▼          ▼",
        "┌───┴──┐   ┌───┴───┐",
        "│ Left │   │ Right │",
        "└───┬──┘   └───┬───┘",
        "    └─────┬────┘",
        "          │",
        "          ▼",
        "      ┌───┴──┐",
        "      │ Join │",
        "      └──────┘",
    ];
    expect(DIAMOND, 80, GOLDEN);
    let rows = rows(&render(DIAMOND, 80).expect("this layout draws the diagram"));
    let start = border_of(&rows, "Start");
    let left = border_of(&rows, "Left");
    let right = border_of(&rows, "Right");
    let join = border_of(&rows, "Join");
    // The split: `Start`'s line drops into one bus row that carries both targets.
    let split = start.bottom + 1;
    assert_eq!(at(&rows, split, start.centre()), '┴');
    assert_eq!(at(&rows, split, left.centre()), '┌');
    assert_eq!(at(&rows, split, right.centre()), '┐');
    // The join: both lines come back into one bus row over `Join`.
    let merge = left.bottom + 1;
    assert_eq!(at(&rows, merge, left.centre()), '└');
    assert_eq!(at(&rows, merge, join.centre()), '┬');
    assert_eq!(at(&rows, merge, right.centre()), '┘');
    // `Start` and `Join` are one layer each (the two boxes of the middle layer are wider), and
    // each layer is centred in the canvas.
    let canvas = rows.iter().map(|row| row.chars().count()).max().unwrap();
    for border in [start, join] {
        let (layer_left, layer_right) = layer_of(&rows, border.top);
        assert_eq!(
            layer_left,
            (canvas - (layer_right - layer_left + 1)) / 2,
            "{border:?} is centred"
        );
    }
}

/// A back edge wraps in a channel outside the canvas (decision 3) instead of crossing a box.
#[test]
fn a_back_edge_wraps_outside_the_canvas() {
    const GOLDEN: &[&str] = &[
        "  ┌──────┐",
        "  │ Read │",
        "  └───┬──┘",
        "      │",
        "      │",
        "      ▼",
        "┌─────┴────┐",
        "│ ◇ Valid? ├◄─┐",
        "└─────┬────┘  │",
        "     ┌┘       │",
        "     │ no     │",
        "     ▼        │",
        "  ┌──┴──┐     │",
        "  │ Fix ├─────┘",
        "  └─────┘",
    ];
    expect(BACK, 80, GOLDEN);
    let rows = rows(&render(BACK, 80).expect("this layout draws the diagram"));
    let valid = border_of(&rows, "◇ Valid?");
    let fix = border_of(&rows, "Fix");
    // The edge back into `◇ Valid?` enters its right border, with a leftward arrowhead.
    assert_eq!(at(&rows, valid.top + 1, valid.right), '├');
    assert_eq!(at(&rows, valid.top + 1, valid.right + 1), '◄');
    // `Fix` is in the box a layer further down, and the channel runs outside every box.
    assert!(fix.top > valid.bottom);
    assert_eq!(at(&rows, fix.top + 1, fix.right), '├');
    // The last vertical of the row above `Fix`'s label row is the channel: it starts level with
    // the target it wraps into and ends at the corner row above `Fix`.
    let channel = rows[fix.top]
        .chars()
        .collect::<Vec<char>>()
        .iter()
        .rposition(|ch| *ch == '│')
        .expect("the channel is the last vertical of the row");
    assert!(channel > fix.right && channel > valid.right);
    // The channel's vertical runs between the two corner rows: the target's label row carries
    // the corner into the box, the source's carries the corner out of it.
    assert!(
        (valid.top + 2..=fix.top).all(|row| at(&rows, row, channel) == '│'),
        "the channel is one vertical line outside the boxes"
    );
}

/// `LR` is the same layout turned: the boxes stand in a row and the arrows point right.
#[test]
fn an_lr_chain_is_the_same_drawing_turned() {
    const GOLDEN: &[&str] = &[
        "┌───────┐   ┌───────┐   ┌──────┐",
        "│ Input ├──►┤ Parse ├──►┤ Draw │",
        "└───────┘   └───────┘   └──────┘",
    ];
    expect(LR_CHAIN, 80, GOLDEN);
    let rows = rows(&render(LR_CHAIN, 80).expect("this layout draws the diagram"));
    let input = border_of(&rows, "Input");
    let parse = border_of(&rows, "Parse");
    assert_eq!(
        input.top, parse.top,
        "the boxes of one row share their rows"
    );
    assert!(parse.left > input.right, "the next layer is to the right");
    // The edge leaves the box's far side, crosses the gap and points at the next border.
    let gap = input.right + 1;
    assert_eq!(at(&rows, input.top + 1, gap), '─');
    assert_eq!(at(&rows, parse.top + 1, parse.left - 1), '►');
}

/// The layout is a pure function of `(source, width)`: a diagram that needs more than the width
/// falls back to the vendor (whose reason the caller keeps the source for), and the same source
/// fits at the wider one.
#[test]
fn the_width_decides_between_the_vendored_reason_and_the_drawing() {
    assert_eq!(
        render(FIVE_LONG, 80).expect_err("too wide for 80 columns"),
        vendored(FIVE_LONG, 80).expect_err("too wide for 80 columns"),
    );
    const GOLDEN: &[&str] = &[
        "┌────────────────┐   ┌────────────────┐   ┌────────────────┐   ┌────────────────┐   ┌────────────────┐",
        "│ aaaaaaaaaaaaaa ├──►┤ bbbbbbbbbbbbbb ├──►┤ cccccccccccccc ├──►┤ dddddddddddddd ├──►┤ eeeeeeeeeeeeee │",
        "└────────────────┘   └────────────────┘   └────────────────┘   └────────────────┘   └────────────────┘",
    ];
    expect(FIVE_LONG, 120, GOLDEN);
}

/// ADR 0028: whatever the layout cannot draw goes to the vendored
/// renderer, and the result is what the transcript showed before this revision — byte for byte,
/// reason included.
#[test]
fn a_diagram_the_layout_refuses_is_the_vendored_render_byte_for_byte() {
    for source in [
        // Not a flowchart: the vendor's family.
        SEQUENCE,
        "stateDiagram-v2\n    [*] --> Still\n    Still --> [*]\n",
        "classDiagram\n    class Animal\n",
        "erDiagram\n    CAR ||--o{ NAMED-DRIVER : allows\n",
        // A flowchart outside the subset.
        SELF_LOOP,
        "flowchart TD\n    subgraph X\n    A\n    end\n",
        "flowchart TD\n    A[one] <--> B[two]\n",
        // Two edges of one gap that would cross: one row cannot tell a crossing from a join.
        "flowchart TD\n    A[one] & B[two] & C[three] & D[four]\n    A --> D\n    C --> B\n",
        // A back edge that would have to cross the box beside its target: the run cannot reach
        // the target's side without going through `two`.
        "flowchart TD\n    A[one] & B[two]\n    A --> C[three]\n    C --> A\n",
        // A labelled edge that skips a layer has no home beside a channel.
        "flowchart TD\n    A[one] --> B[two] --> C[three]\n    A -->|skip| C\n",
        // Wide for this width.
        FIVE_LONG,
        // Rejected by both.
        "flowchart TD; A[one]; A[two]",
    ] {
        let ours = render(source, 80);
        let vendor = vendored(source, 80);
        match (ours, vendor) {
            (Ok(ours), Ok(vendor)) => assert_eq!(ours, vendor, "{source:?}"),
            (Err(ours), Err(vendor)) => assert_eq!(ours, vendor, "{source:?}"),
            (ours, vendor) => panic!("{source:?}: {ours:?} is not the vendor's {vendor:?}"),
        }
    }
}

/// ADR 0028: the layout reads the same grammar as the vendor, so it must
/// never accept a source the vendor rejects — a wrong acceptance would draw a diagram where the
/// transcript used to keep its source. (The other direction needs no pinning: every refusal
/// falls back to the vendor, so a refusal can only reproduce today's output.)
///
/// The rejected samples are the vendor's own (`third_party/codex-mermaid/src/tests.rs`).
#[test]
fn accepts_the_same_samples_as_the_vendored_renderer() {
    let accepted = [
        "flowchart TD\n    A[Start] --> B[Done]\n",
        "graph LR; A --> B --> C",
        "flowchart\n    A --> B",
        "graph TB; A --> B",
        "flowchart BT; A --> B",
        "flowchart RL; A --> B",
        "flowchart TD; A([Ready?]) -- go --> B[Work] & C{Done?}",
        "flowchart TD; B -. no .-> C",
        "flowchart TD; A <--> B",
        "flowchart TD; A -.-> B",
        "flowchart TD; A -.- B",
        "flowchart TD; A --- B",
        "flowchart TD; A -->|yes| B",
        "flowchart TD; A -- yes --> B",
        "flowchart TD; A -. retry .-> B",
        "flowchart TD; A --> B & B",
        "flowchart TD; A[R&D] -->|R&D| B[Review & confirm]; B --> C",
        "graph TD; A[\"chimpansen hoppar ()[]\"] -->|\"x | y; z\"| B{\"x < 3?\"};",
        "flowchart TD; A[\"(Database)\"]",
        "flowchart TD; A[\"/Input/\"]",
        "% comment\nflowchart TD; A --> B",
        "flowchart TD; A[请求] --> B[工作]",
        "flowchart TD\n    A[one] --> B[two]",
    ];
    let rejected = [
        "flowchart TD; P --> Q; A[(Database)]",
        "flowchart TD; P --> Q; A[/Input/]",
        "flowchart TD; P --> Q; A[[Subroutine]]",
        "flowchart TD; P --> Q; A{{Hexagon}}",
        "flowchart TD; P --> Q; A[\"`hello **world**`\"]",
        "flowchart TD; subgraph X; A; end",
        "flowchart TD; A --> B; garbage syntax",
        "flowchart TD; A --> B &",
        "flowchart TD; A && B",
        "flowchart TD; A -- Yes B",
        "flowchart TD; A -. retry --> B",
        "flowchart TD; A ==> B",
        "flowchart TD; A -- hello --- B --> C",
        "flowchart TD; A -- hello o--> B",
        "flowchart TD; A --oB --> C",
        "flowchart TD; A---oB",
        "flowchart TD; A-.-xB",
        "flowchart TD; A[one]; A[two]",
        "flowchart TD; A[<b>HTML</b>]",
        "flowchart TD; A[&#27;]",
        "flowchart TD; A[e\u{301}]",
        "flowchart TD; A[👍🏽]",
        "flowchart TD; A[👩\u{200d}💻]",
        "flowchart TD; A[zero\u{200b}width]",
        "flowchart TD; A[unclosed",
        "flowchart TD; click A",
        "flowchart TD; A((circle))",
        "flowchart TD; A(rounded)",
        "flowchart TD; A([unclosed]",
        "flowchart TD; A([label]) trailing",
        "flowchart TD; A([])",
        "flowchart TD; A([nested[label]])",
        "flowchart TD; A([one]); A([two])",
        "flowchart TD; A([same]); A[same]",
        "flowchart TD; A{same}; A([same])",
        "flowchart TD; A -->|unclosed B",
        "flowchart TD; A -->|yes┐| B",
        "flowchart TD; A[one] --> B[two] --> C[three]",
        "flowchart TD; A --> B; A --> B; A --> B",
        "flowchart TD; A --> B; B --> C; C --> A",
    ];
    for source in accepted.iter().chain(rejected.iter()) {
        assert_eq!(
            render(source, 120).is_ok(),
            vendored(source, 120).is_ok(),
            "{source:?}"
        );
    }
    // The layout, not the vendor, drew the snapshots this file pins: the two disagree on them,
    // and the only two producers of `render` are this layout and the vendor.
    for source in [CHAIN, DIAMOND, BACK, LR_CHAIN] {
        assert_ne!(
            render(source, 120).expect("this layout draws it"),
            vendored(source, 120).expect("the vendor draws it"),
            "{source:?} must be this layout's drawing"
        );
    }
}

/// The published output keeps ADR 0028 R8's contract: only the three roles the theme knows, and
/// never an escape sequence — used the way the transcript's theme expects (a box's border is a
/// node, an edge's label belongs to its edge, a node's label is plain text).
#[test]
fn the_layout_emits_only_the_three_roles() {
    use codex_mermaid::Role;
    for source in [CHAIN, DIAMOND, BACK, LR_CHAIN] {
        let lines = render_spans(source, 80).expect("this layout draws the diagram");
        assert!(!lines.is_empty());
        for span in lines.iter().flatten() {
            assert!(
                !span.text.contains('\x1b'),
                "ANSI escape in {:?}",
                span.text
            );
            assert!(
                matches!(span.role, Role::Node | Role::Edge | Role::Text),
                "unknown role {:?}",
                span.role
            );
        }
    }
    let lines = render_spans(CHAIN, 80).expect("this layout draws the diagram");
    let role_of = |needle: &str| {
        lines
            .iter()
            .flatten()
            .find(|span| span.text.contains(needle))
            .unwrap_or_else(|| panic!("no span carries {needle:?}"))
            .role
    };
    assert_eq!(role_of("User input"), Role::Text, "a node's label");
    assert_eq!(role_of("yes"), Role::Edge, "an edge's label");
    assert_eq!(role_of("┌"), Role::Node, "a box's border");
}

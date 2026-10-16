//! Radian's own layered layout for Mermaid flowcharts (ADR 0028).
//!
//! Revision 6 gave `flowchart`/`graph` a second renderer: this module lays the parsed graph out
//! itself — layers centred in the canvas, one-row-high boxes, a straight line with an arrowhead
//! between the nodes of two adjacent layers, a horizontal bus where a layer forks or merges, and
//! a channel outside the canvas for an edge that skips a layer or points back — and
//! [`radian_host::mermaid`](super::mermaid) falls back to the vendored `codex-mermaid` for
//! everything else (sequences, states, classes, ER, and any flowchart this layout refuses).
//! `third_party/codex-mermaid` is not modified (R8: vendored unchanged); this is a second reader
//! of the same grammar, so `accepts_the_same_samples_as_the_vendored_renderer` in
//! `tests/mermaid_layout.rs` pins that it never accepts a source the vendor rejects, and that it
//! rejects every sample the vendor rejects.
//!
//! The subset, and why it is narrower than the vendor's:
//!
//! - `flowchart`/`graph` with `TD`/`TB`/`BT`/`LR`/`RL`.
//! - Rectangle, decision and stadium nodes; quoted and unquoted labels; solid and dashed edges;
//!   pipe labels, infix labels, and `&` fan-out/fan-in.
//! - No self-loop, and no edge with a marker at its source end (`<-->`, `<-.->`): the layout has
//!   no home for either (the vendor draws them).
//! - A labelled edge that skips a layer or points back would need its label beside the channel,
//!   where there is no room; the whole diagram goes to the vendor.
//! - Two edges of one gap whose bus runs overlap *without* sharing an endpoint would cross, and
//!   a crossing cannot be told from a join in one row: that is "cannot draw", not a guess.
//! - Any collision (a label with no free cells, a wrapping run that would cross a box) is also
//!   "cannot draw". Every refusal falls back to the vendor, so a refusal costs the caller
//!   nothing but today's rendering.

use codex_mermaid::{Role, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The vendor's own caps, in step with `third_party/codex-mermaid/src/lib.rs`: a source over one
/// of them is the vendor's to reject (with its own reason), not ours to draw.
const MAX_SOURCE: usize = 16 * 1024;
const MAX_NODES: usize = 16;
const MAX_EDGES: usize = 24;
const MAX_LABEL: usize = 40;
const MAX_CELLS: usize = 64 * 1024;

/// The gap between two layers, in the layer axis: the bus row, the label row, the arrow row.
const GAP: usize = 3;
/// The free columns between two boxes of one layer, in the across axis.
const BOX_GAP: usize = 3;
/// The wrapping channels start this far outside the layers' region, and every further channel is
/// two units out (one free column between two channels).
const CHANNEL_MARGIN: usize = 2;

// The four wire directions, in layout space (see `Flow`): the across axis is the axis a layer's
// boxes are laid out on, the along axis is the direction layers advance in.
const ACROSS_POS: u8 = 1;
const ACROSS_NEG: u8 = 2;
const ALONG_POS: u8 = 4;
const ALONG_NEG: u8 = 8;

/// The direction the layers advance in. `Down` is `TD`/`TB`, `Up` is `BT`, `Right` is `LR` and
/// `Left` is `RL`; everything is laid out in this space and turned to the screen at the end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Rectangle,
    Decision,
    Stadium,
}

/// Where an edge's arrowhead is drawn. The vendor also marks an edge's source end; the layout
/// has no place for that, so such an edge is not ours (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tip {
    Target,
    None,
}

struct Node {
    id: String,
    label: String,
    shape: Shape,
    declared: bool,
}

struct Edge {
    from: usize,
    to: usize,
    label: String,
    dashed: bool,
    tip: Tip,
}

struct Graph {
    flow: Flow,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl Graph {
    /// The vendor's `Graph::node`: an id seen for the first time becomes an undeclared node
    /// whose label is its own id.
    fn node(&mut self, id: &str) -> Option<usize> {
        if let Some(index) = self.nodes.iter().position(|node| node.id == id) {
            return Some(index);
        }
        if self.nodes.len() == MAX_NODES {
            return None;
        }
        self.nodes.push(Node {
            id: id.to_owned(),
            label: id.to_owned(),
            shape: Shape::Rectangle,
            declared: false,
        });
        Some(self.nodes.len() - 1)
    }
}

/// Draws `source` with Radian's layered flowchart layout, or `None` when the source is not a
/// flowchart of the supported subset or the layout cannot be drawn inside `max_width` cells.
pub(super) fn render(source: &str, max_width: usize) -> Option<Vec<Vec<Span>>> {
    let graph = parse(source)?;
    let plan = plan(&graph)?;
    if plan.width() > max_width || plan.across() * plan.along > MAX_CELLS {
        return None;
    }
    let grid = draw(&graph, &plan)?;
    Some(spans(&grid, &plan))
}

// --- parsing, mirroring `third_party/codex-mermaid/src/parse.rs` ---

/// Reads the flowchart grammar; `None` means "not this layout's source" (the vendor decides).
fn parse(source: &str) -> Option<Graph> {
    if source.len() > MAX_SOURCE {
        return None;
    }
    let statements = statements(source)?;
    let (header, body) = statements.split_first()?;
    let flow = match header.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["flowchart" | "graph"] => Flow::Down,
        ["flowchart" | "graph", "TD" | "TB"] => Flow::Down,
        ["flowchart" | "graph", "BT"] => Flow::Up,
        ["flowchart" | "graph", "LR"] => Flow::Right,
        ["flowchart" | "graph", "RL"] => Flow::Left,
        _ => return None,
    };
    let mut graph = Graph {
        flow,
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    for statement in body {
        let mut rest = *statement;
        let mut from = node_list(&mut rest, &mut graph)?;
        while !rest.trim_start().is_empty() {
            rest = rest.trim_start();
            let mut label = String::new();
            let (tip, dashed) = if let Some((after, tip, dashed)) = [
                ("<-.->", None, true),
                ("<-->", None, false),
                ("-.->", Some(Tip::Target), true),
                ("-.-", Some(Tip::None), true),
                ("-->", Some(Tip::Target), false),
                ("---", Some(Tip::None), false),
            ]
            .into_iter()
            .find_map(|(token, tip, dashed)| {
                rest.strip_prefix(token).map(|after| (after, tip, dashed))
            }) {
                // An edge marked at its source end, or one whose target end is a circle or a
                // cross, is not this layout's (the vendor renders the first; both reject the
                // second when the marker is attached to an id).
                let tip = tip?;
                if tip == Tip::None && after.starts_with(['o', 'x']) {
                    return None;
                }
                rest = after.trim_start();
                if let Some(after) = rest.strip_prefix('|') {
                    let (text, remaining) = delimited(after, "|")?;
                    label = label_text(text)?.to_owned();
                    rest = remaining;
                }
                (tip, dashed)
            } else {
                let (after, stem, dashed) = if let Some(after) = rest.strip_prefix("--") {
                    (after, "--", false)
                } else {
                    (rest.strip_prefix("-.")?, ".-", true)
                };
                // A spaced label keeps endpoint markers out of its text, and an unsupported
                // edge and its target are not swallowed: the closing stem must be there.
                let (text, remaining) = after.split_once(stem)?;
                if !text.starts_with(char::is_whitespace) || !text.ends_with(char::is_whitespace) {
                    return None;
                }
                rest = remaining.strip_prefix('>')?;
                label = label_text(text.trim())?.to_owned();
                (Tip::Target, dashed)
            };
            let to = node_list(&mut rest, &mut graph)?;
            // The Cartesian expansion is checked before the edges are allocated, as the vendor
            // does, so `&` groups cannot pass the edge cap.
            if from.len() * to.len() > MAX_EDGES - graph.edges.len() {
                return None;
            }
            for &from in &from {
                for &to in &to {
                    graph.edges.push(Edge {
                        from,
                        to,
                        label: label.clone(),
                        dashed,
                        tip,
                    });
                }
            }
            from = to;
        }
    }
    if graph.nodes.is_empty() {
        return None;
    }
    Some(graph)
}

/// Splits the source into statements the way the vendor's `syntax::statements` does for a
/// flowchart: `%%` comments, `;` separators outside quotes and label delimiters, and a rejection
/// for the entity escapes the vendor's decoder does not implement. Only the flowchart family is
/// modelled here (the header is checked afterwards), so the class-body and `:` rules are absent.
fn statements(source: &str) -> Option<Vec<&str>> {
    let mut result: Vec<&str> = Vec::new();
    for line in source.lines() {
        let mut rest = line.trim();
        if rest.starts_with("%%{") {
            return None;
        }
        if rest.starts_with("%%") {
            continue;
        }
        if rest.match_indices(['#', '&']).any(|(index, _)| {
            let after = &rest[index + 1..];
            let len = after
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            len > 0 && after[len..].starts_with(';')
        }) {
            return None;
        }
        while !rest.is_empty() {
            let flowchart = matches!(
                result
                    .first()
                    .copied()
                    .unwrap_or("")
                    .split_whitespace()
                    .next(),
                Some("flowchart" | "graph")
            );
            let mut close = None;
            let mut quoted = false;
            let end = rest.char_indices().find_map(|(index, ch)| {
                match ch {
                    '"' => quoted = !quoted,
                    ';' if !quoted && close.is_none() => return Some(index),
                    ch if flowchart && !quoted => {
                        close = match (close, ch) {
                            (Some(end), ch) if end == ch => None,
                            (None, '[') => Some(']'),
                            (None, '{') => Some('}'),
                            (None, '|') => Some('|'),
                            (close, _) => close,
                        };
                    }
                    _ => {}
                }
                None
            });
            let (statement, remaining) = match end {
                Some(end) => (&rest[..end], &rest[end + 1..]),
                None => (rest, ""),
            };
            if !statement.trim().is_empty() {
                result.push(statement.trim());
            }
            rest = remaining.trim();
        }
    }
    Some(result)
}

/// One node reference, then the `&`-separated ones after it (the vendor's `nodes`).
fn node_list(rest: &mut &str, graph: &mut Graph) -> Option<Vec<usize>> {
    let mut nodes = vec![node(rest, graph)?];
    while let Some(after) = rest.trim_start().strip_prefix('&') {
        if nodes.len() == MAX_EDGES {
            return None;
        }
        *rest = after;
        nodes.push(node(rest, graph)?);
    }
    Some(nodes)
}

/// One node: its id, and its shape declaration when it carries one (the vendor's `node`).
fn node(rest: &mut &str, graph: &mut Graph) -> Option<usize> {
    let id = identifier(rest)?;
    if matches!(
        id,
        "end" | "subgraph" | "direction" | "style" | "class" | "classDef" | "linkStyle" | "click"
    ) {
        return None;
    }
    // Longer shape delimiters must not become punctuation inside a simpler node.
    if ["[(", "[[", "[/", "[\\", "{{"]
        .iter()
        .any(|open| rest.starts_with(open))
    {
        return None;
    }
    let declaration = match rest.chars().next() {
        Some('[') => Some(("[", "]", Shape::Rectangle)),
        Some('{') => Some(("{", "}", Shape::Decision)),
        Some('(') if rest.starts_with("([") => Some(("([", "])", Shape::Stadium)),
        _ => None,
    };
    let index = graph.node(id)?;
    if let Some((open, close, shape)) = declaration {
        let (label, remaining) = delimited(&rest[open.len()..], close)?;
        let label = label_text(label)?;
        *rest = remaining;
        let node = &mut graph.nodes[index];
        if node.declared && (node.label != label || node.shape != shape) {
            return None;
        }
        node.label = label.to_owned();
        node.shape = shape;
        node.declared = true;
    }
    Some(index)
}

/// Consumes a label up to its closing delimiter, protecting delimiters inside a leading quoted
/// token (the vendor's `delimited_label`).
fn delimited<'a>(text: &'a str, close: &str) -> Option<(&'a str, &'a str)> {
    if let Some(quoted) = text.strip_prefix('"') {
        let end = quoted.find('"')? + 2;
        let rest = text[end..].strip_prefix(close)?;
        Some((&text[..end], rest))
    } else {
        text.split_once(close)
    }
}

/// The label rules the vendor applies (`flowchart_label`): quotes are optional and protect
/// delimiters, and the result must be text this renderer can measure and draw.
fn label_text(label: &str) -> Option<&str> {
    if label.starts_with("\"`") {
        return None;
    }
    let label = if let Some(quoted) = label.strip_prefix('"') {
        quoted.strip_suffix('"')?
    } else {
        if label.contains(['[', ']', '{', '}', '|']) {
            return None;
        }
        label
    };
    if label.contains('"') {
        return None;
    }
    check_label(label)?;
    Some(label)
}

/// The vendor's `check_label`: no markup, no control or zero-width characters, no ligature whose
/// width is not the sum of its scalars' widths, and at most `MAX_LABEL` display cells.
fn check_label(label: &str) -> Option<()> {
    let markup = label.match_indices('<').any(|(index, _)| {
        let after = &label[index + 1..];
        after.starts_with(|ch: char| ch.is_ascii_alphabetic() || matches!(ch, '/' | '!' | '?'))
    });
    if markup
        || label.trim().is_empty()
        || label.chars().any(|ch| {
            ch.is_control()
                || matches!(ch, '┌' | '┐' | '└' | '┘' | '├' | '┤' | '╪' | '◄')
                || UnicodeWidthChar::width(ch).is_none_or(|width| width == 0)
        })
    {
        return None;
    }
    if label
        .chars()
        .filter_map(UnicodeWidthChar::width)
        .sum::<usize>()
        != UnicodeWidthStr::width(label)
    {
        return None;
    }
    if UnicodeWidthStr::width(label) > MAX_LABEL {
        return None;
    }
    Some(())
}

/// The vendor's `identifier`: ASCII letters first, then letters, digits and `_`, at most
/// `MAX_LABEL` bytes.
fn identifier<'a>(rest: &mut &'a str) -> Option<&'a str> {
    *rest = rest.trim_start();
    let len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    let id = &rest[..len];
    if id.is_empty() || !id.as_bytes()[0].is_ascii_alphabetic() || id.len() > MAX_LABEL {
        return None;
    }
    *rest = &rest[len..];
    Some(id)
}

// --- the layout ---

/// A box's place in layout space: its across and along start, and its extent on each axis.
#[derive(Clone, Copy)]
struct Placed {
    across: usize,
    along: usize,
    across_size: usize,
    along_size: usize,
}

impl Placed {
    /// The last column of the box in the across axis.
    fn across_end(&self) -> usize {
        self.across + self.across_size - 1
    }

    /// The box's middle row in the along axis.
    fn mid_along(&self) -> usize {
        self.along + self.along_size / 2
    }

    /// The box's middle column in the across axis: where a straight line meets it.
    fn centre(&self) -> usize {
        self.across + self.across_size / 2
    }
}

/// A parsed graph with every place worked out. The separate `across` region is what the layers
/// are centred in; the wrapping channels sit outside it (revision 6 decision 3), and `across()`
/// is the whole canvas.
struct Plan {
    flow: Flow,
    nodes: Vec<Placed>,
    /// Per edge: does it run between two adjacent layers (the straight-line case)?
    adjacent: Vec<bool>,
    /// Per node: its layer.
    layer_of: Vec<usize>,
    along: usize,
    across_layers: usize,
    channels: usize,
}

impl Plan {
    fn horizontal(&self) -> bool {
        matches!(self.flow, Flow::Right | Flow::Left)
    }

    fn across(&self) -> usize {
        if self.channels == 0 {
            self.across_layers
        } else {
            self.across_layers + 2 * self.channels + 1
        }
    }

    /// The canvas width in screen cells: what `max_width` bounds.
    fn width(&self) -> usize {
        if self.horizontal() {
            self.along
        } else {
            self.across()
        }
    }

    /// The channel column of the `index`-th wrapping edge.
    fn channel(&self, index: usize) -> usize {
        self.across_layers + CHANNEL_MARGIN + 2 * index
    }
}

/// Works out the layers, the order inside them and every box's place, or `None` when the graph
/// is outside the subset (a self-loop) or has no place for its edges.
fn plan(graph: &Graph) -> Option<Plan> {
    if graph.edges.iter().any(|edge| edge.from == edge.to) {
        return None;
    }
    let (layers, layer_of, back) = layer(graph);
    let horizontal = matches!(graph.flow, Flow::Right | Flow::Left);
    // The vendor's `◇ ` marker makes a decision box two cells wider than its label.
    let labels: Vec<String> = graph
        .nodes
        .iter()
        .map(|node| match node.shape {
            Shape::Decision => format!("◇ {}", node.label),
            Shape::Rectangle | Shape::Stadium => node.label.clone(),
        })
        .collect();
    let widths: Vec<usize> = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .collect();
    // A one-row-high box around its label: `┌────┐` / `│ label │` / `└────┘`. In the layer axis
    // that is three units; in the label's own axis it is the label plus a space and a border on
    // each side.
    let across_size: Vec<usize> = widths
        .iter()
        .map(|width| if horizontal { 3 } else { width + 4 })
        .collect();
    let along_size: Vec<usize> = widths
        .iter()
        .map(|width| if horizontal { width + 4 } else { 3 })
        .collect();
    let layer_across: Vec<usize> = layers
        .iter()
        .map(|nodes| {
            let boxes: usize = nodes.iter().map(|&node| across_size[node]).sum();
            boxes + BOX_GAP * nodes.len().saturating_sub(1)
        })
        .collect();
    let mut starts = Vec::with_capacity(layers.len());
    let mut along = 0;
    for nodes in &layers {
        let extent = nodes
            .iter()
            .map(|&node| along_size[node])
            .max()
            .unwrap_or(0);
        starts.push(along);
        along += extent + GAP;
    }
    let along = along.saturating_sub(GAP);
    let adjacent: Vec<bool> = graph
        .edges
        .iter()
        .enumerate()
        .map(|(index, edge)| !back[index] && layer_of[edge.to] == layer_of[edge.from] + 1)
        .collect();
    let channels = adjacent.iter().filter(|&&adjacent| !adjacent).count();
    // A label beside a line can reach past the layers' region, which grows the region and moves
    // every layer (they stay centred). The region only ever grows, so the loop settles.
    let mut across_layers = layer_across.iter().copied().max().unwrap_or(0);
    let mut along = along;
    for _ in 0..8 {
        let placed = place(
            graph,
            &layers,
            &layer_across,
            &across_size,
            &along_size,
            &starts,
            across_layers,
        );
        let (need_across, need_along) =
            label_room(graph, &placed, &adjacent, horizontal, &layer_of, &starts);
        let across = across_layers.max(need_across);
        let end = along.max(need_along);
        if across == across_layers && end == along {
            break;
        }
        across_layers = across;
        along = end;
    }
    let within_layers = place(
        graph,
        &layers,
        &layer_across,
        &across_size,
        &along_size,
        &starts,
        across_layers,
    );
    Some(Plan {
        flow: graph.flow,
        nodes: within_layers,
        adjacent,
        layer_of,
        along,
        across_layers,
        channels,
    })
}

/// Splits the graph into layers, marking the edges that point back at a node still on the DFS
/// stack. The remaining edges form a DAG, and a node's layer is the longest path to it.
fn layer(graph: &Graph) -> (Vec<Vec<usize>>, Vec<usize>, Vec<bool>) {
    let count = graph.nodes.len();
    let mut out = vec![Vec::new(); count];
    for (index, edge) in graph.edges.iter().enumerate() {
        out[edge.from].push(index);
    }
    let mut visited = vec![false; count];
    let mut on_stack = vec![false; count];
    let mut back = vec![false; graph.edges.len()];
    let mut finished = Vec::with_capacity(count);
    for start in 0..count {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        on_stack[start] = true;
        let mut stack = vec![(start, 0usize)];
        while let Some(&(node, cursor)) = stack.last() {
            if let Some(&edge) = out[node].get(cursor) {
                if let Some(frame) = stack.last_mut() {
                    frame.1 += 1;
                }
                let to = graph.edges[edge].to;
                if on_stack[to] {
                    back[edge] = true;
                } else if !visited[to] {
                    visited[to] = true;
                    on_stack[to] = true;
                    stack.push((to, 0));
                }
            } else {
                on_stack[node] = false;
                finished.push(node);
                stack.pop();
            }
        }
    }
    let mut layer_of = vec![0usize; count];
    for &node in finished.iter().rev() {
        for &edge in &out[node] {
            if back[edge] {
                continue;
            }
            let to = graph.edges[edge].to;
            layer_of[to] = layer_of[to].max(layer_of[node] + 1);
        }
    }
    let mut layers = vec![Vec::new(); layer_of.iter().copied().max().unwrap_or(0) + 1];
    for (node, &index) in layer_of.iter().enumerate() {
        layers[index].push(node);
    }
    (layers, layer_of, back)
}

/// Every box's across start: each layer centred in `canvas`, its boxes in declaration order.
fn place(
    graph: &Graph,
    layers: &[Vec<usize>],
    layer_across: &[usize],
    across_size: &[usize],
    along_size: &[usize],
    starts: &[usize],
    canvas: usize,
) -> Vec<Placed> {
    let mut placed = vec![
        Placed {
            across: 0,
            along: 0,
            across_size: 0,
            along_size: 0,
        };
        graph.nodes.len()
    ];
    for (index, nodes) in layers.iter().enumerate() {
        let mut across = (canvas - layer_across[index]) / 2;
        for &node in nodes {
            placed[node] = Placed {
                across,
                along: starts[index],
                across_size: across_size[node],
                along_size: along_size[node],
            };
            across += across_size[node] + BOX_GAP;
        }
    }
    placed
}

/// How wide and how long the canvas must be for every adjacent labelled edge's label to have a
/// home beside its line: to the right of the line in the vertical layout, and on the free row
/// under the target's box in the horizontal one.
fn label_room(
    graph: &Graph,
    placed: &[Placed],
    adjacent: &[bool],
    horizontal: bool,
    layer_of: &[usize],
    starts: &[usize],
) -> (usize, usize) {
    let mut across = 0;
    let mut along = 0;
    for (index, edge) in graph.edges.iter().enumerate() {
        if !adjacent[index] || edge.label.is_empty() {
            continue;
        }
        let width = UnicodeWidthStr::width(edge.label.as_str());
        let target = placed[edge.to].centre();
        if horizontal {
            across = across.max(target + 3);
            along = along.max(starts[layer_of[edge.to]] + 2 + width);
        } else {
            across = across.max(target + 2 + width);
        }
    }
    (across, along)
}

/// One canvas cell: the wire directions that meet there, whether those wires are dashed, a
/// marker's own direction, the semantic role the cell is drawn with, and — for a stadium box's
/// corners — a glyph that replaces what the wire directions would give.
#[derive(Clone, Copy, Default)]
struct Cell {
    bits: u8,
    dashed: u8,
    arrow: u8,
    glyph: Option<char>,
    text: bool,
    role: Option<Role>,
}

/// A run of text (a node's label, an edge's label): `runs_along` says which axis its characters
/// advance on. Text always reads left to right in the finished picture, whatever the direction.
struct TextRun {
    across: usize,
    along: usize,
    runs_along: bool,
    text: String,
    role: Role,
}

struct Grid {
    across: usize,
    along: usize,
    cells: Vec<Cell>,
    text: Vec<TextRun>,
}

impl Grid {
    fn new(across: usize, along: usize) -> Self {
        Self {
            across,
            along,
            cells: vec![Cell::default(); across * along],
            text: Vec::new(),
        }
    }

    fn cell(&self, across: usize, along: usize) -> Cell {
        self.cells[along * self.across + across]
    }

    fn put(&mut self, across: usize, along: usize, cell: Cell) {
        let index = along * self.across + across;
        self.cells[index] = cell;
    }

    fn free(&self, across: usize, along: usize) -> bool {
        let cell = self.cell(across, along);
        cell.bits == 0 && cell.arrow == 0 && !cell.text
    }

    /// A stadium node's four corners, rounded as the vendor draws them. The glyph comes from the
    /// corner's own wire directions, so a turned layout (`LR`/`RL`) gets the right corners.
    fn round_corners(&mut self, node: &Placed, flow: Flow) {
        let (left, right) = (node.across, node.across_end());
        let (top, bottom) = (node.along, node.along + node.along_size - 1);
        for (across, along) in [(left, top), (right, top), (left, bottom), (right, bottom)] {
            let mut cell = self.cell(across, along);
            if let Some(glyph) = round_glyph(screen_bits(cell.bits, flow)) {
                cell.glyph = Some(glyph);
                self.put(across, along, cell);
            }
        }
    }

    /// Adds wire directions at one cell. A box's border keeps its node role where a wire meets
    /// it (the vendor marks such a cell `├`); anything drawn over text is a collision.
    fn wire(
        &mut self,
        across: usize,
        along: usize,
        bits: u8,
        dashed: bool,
        role: Role,
    ) -> Option<()> {
        let mut cell = self.cell(across, along);
        if cell.arrow != 0 || cell.text {
            return None;
        }
        cell.bits |= bits;
        if dashed {
            cell.dashed |= bits;
        }
        if cell.role.is_none() {
            cell.role = Some(role);
        }
        self.put(across, along, cell);
        Some(())
    }

    fn wire_path(&mut self, path: &[(usize, usize)], dashed: bool, role: Role) -> Option<()> {
        for step in path.windows(2) {
            let (across, along) = step[0];
            let (next_across, next_along) = step[1];
            let forward = direction(across, along, next_across, next_along)?;
            let backward = direction(next_across, next_along, across, along)?;
            self.wire(across, along, forward, dashed, role)?;
            self.wire(next_across, next_along, backward, dashed, role)?;
        }
        Some(())
    }

    /// Puts an arrowhead pointing `direction` (a layout-space direction, turned to the screen in
    /// `spans`). Two edges into one node share their arrowhead, so a second call here is not a
    /// collision.
    fn arrow(&mut self, across: usize, along: usize, direction: u8) -> Option<()> {
        let mut cell = self.cell(across, along);
        if cell.arrow != 0 {
            return Some(());
        }
        if cell.bits != 0 || cell.text {
            return None;
        }
        cell.arrow = direction;
        cell.role = Some(Role::Edge);
        self.put(across, along, cell);
        Some(())
    }

    /// Writes a label, failing when any cell it would cover is taken or outside the canvas: the
    /// caller then tries the other side, and the diagram goes to the vendor if both are blocked.
    fn label(
        &mut self,
        across: usize,
        along: usize,
        runs_along: bool,
        text: &str,
        role: Role,
    ) -> Option<()> {
        let width = UnicodeWidthStr::width(text);
        for step in 0..width {
            let (across, along) = if runs_along {
                (across, along + step)
            } else {
                (across + step, along)
            };
            if across >= self.across || along >= self.along || !self.free(across, along) {
                return None;
            }
        }
        for step in 0..width {
            let (across, along) = if runs_along {
                (across, along + step)
            } else {
                (across + step, along)
            };
            let mut cell = self.cell(across, along);
            cell.text = true;
            cell.role = Some(role);
            self.put(across, along, cell);
        }
        self.text.push(TextRun {
            across,
            along,
            runs_along,
            text: text.to_owned(),
            role,
        });
        Some(())
    }
}

/// The wire direction from one cell of a path to the next; `None` when they are not neighbours.
fn direction(across: usize, along: usize, next_across: usize, next_along: usize) -> Option<u8> {
    if next_across == across && next_along == along + 1 {
        Some(ALONG_POS)
    } else if next_across == across && next_along + 1 == along {
        Some(ALONG_NEG)
    } else if next_along == along && next_across == across + 1 {
        Some(ACROSS_POS)
    } else if next_along == along && next_across + 1 == across {
        Some(ACROSS_NEG)
    } else {
        None
    }
}

/// Draws every box, then every edge, or `None` on the first collision.
fn draw(graph: &Graph, plan: &Plan) -> Option<Grid> {
    bus_rows_are_clear(graph, plan)?;
    let mut grid = Grid::new(plan.across(), plan.along);
    for (index, node) in graph.nodes.iter().enumerate() {
        let placed = plan.nodes[index];
        grid.wire_path(&box_path(&placed), false, Role::Node)?;
        let label = match node.shape {
            Shape::Decision => format!("◇ {}", node.label),
            Shape::Rectangle | Shape::Stadium => node.label.clone(),
        };
        // A stadium keeps the vendor's rounded corners (ADR 0028); every
        // other shape is square.
        if node.shape == Shape::Stadium {
            grid.round_corners(&placed, plan.flow);
        }
        // A one-row-high box: the label sits on the middle line of the layer axis, and reads
        // along the axis that is the screen's horizontal one.
        let run_across = !plan.horizontal();
        let (across, along) = if run_across {
            (placed.across + 2, placed.mid_along())
        } else {
            (placed.centre(), placed.along + 2)
        };
        grid.label(across, along, !run_across, &label, Role::Text)?;
    }
    let mut channel = 0;
    for (index, edge) in graph.edges.iter().enumerate() {
        let from = plan.nodes[edge.from];
        let to = plan.nodes[edge.to];
        if plan.adjacent[index] {
            adjacent_edge(&mut grid, from, to, edge, plan.horizontal())?;
        } else {
            channel_edge(&mut grid, from, to, edge, plan.channel(channel))?;
            channel += 1;
        }
    }
    Some(grid)
}

/// The four border lines of a box, as a closed path, so every corner and every junction where an
/// edge meets the border comes out of the wire directions alone.
fn box_path(node: &Placed) -> Vec<(usize, usize)> {
    let (left, top) = (node.across, node.along);
    let (right, bottom) = (node.across_end(), node.along + node.along_size - 1);
    let mut path = Vec::new();
    for across in left..=right {
        path.push((across, top));
    }
    for along in top + 1..=bottom {
        path.push((right, along));
    }
    for across in (left..right).rev() {
        path.push((across, bottom));
    }
    for along in (top + 1..bottom).rev() {
        path.push((left, along));
    }
    path.push((left, top));
    path
}

/// Whether the bus rows of one gap can be drawn. Two adjacent edges in the same gap may share
/// cells of their bus row only when they share an endpoint — then the runs really join and the
/// junction is honest. Two runs that overlap without one would be a crossing, and one row cannot
/// tell a crossing from a join: the whole diagram goes to the vendor.
fn bus_rows_are_clear(graph: &Graph, plan: &Plan) -> Option<()> {
    // (gap, low column, high column, source, target)
    let mut runs = Vec::new();
    for (index, edge) in graph.edges.iter().enumerate() {
        if !plan.adjacent[index] {
            continue;
        }
        let source = plan.nodes[edge.from].centre();
        let target = plan.nodes[edge.to].centre();
        runs.push((
            plan.layer_of[edge.from],
            source.min(target),
            source.max(target),
            edge.from,
            edge.to,
        ));
    }
    for (index, run) in runs.iter().enumerate() {
        for other in &runs[index + 1..] {
            if run.0 != other.0 || run.1 > other.2 || other.1 > run.2 {
                continue;
            }
            if run.3 == other.3 || run.4 == other.4 {
                continue;
            }
            return None;
        }
    }
    Some(())
}

/// An edge between two adjacent layers: down the source's middle, across the bus row if the two
/// middles differ, down the target's middle, arrowhead into the target's border.
fn adjacent_edge(
    grid: &mut Grid,
    from: Placed,
    to: Placed,
    edge: &Edge,
    horizontal: bool,
) -> Option<()> {
    let source = from.centre();
    let target = to.centre();
    let bus = from.along + from.along_size;
    let arrow = bus + GAP - 1;
    grid.wire(
        source,
        from.along + from.along_size - 1,
        ALONG_POS,
        edge.dashed,
        Role::Node,
    )?;
    if source == target {
        grid.wire(source, bus, ALONG_POS | ALONG_NEG, edge.dashed, Role::Edge)?;
        grid.wire(
            source,
            bus + 1,
            ALONG_POS | ALONG_NEG,
            edge.dashed,
            Role::Edge,
        )?;
    } else {
        let (low, high) = (source.min(target), source.max(target));
        for across in low..=high {
            let mut bits = 0;
            if across > low {
                bits |= ACROSS_NEG;
            }
            if across < high {
                bits |= ACROSS_POS;
            }
            grid.wire(across, bus, bits, edge.dashed, Role::Edge)?;
        }
        grid.wire(source, bus, ALONG_NEG, edge.dashed, Role::Edge)?;
        grid.wire(target, bus, ALONG_POS, edge.dashed, Role::Edge)?;
        grid.wire(
            target,
            bus + 1,
            ALONG_POS | ALONG_NEG,
            edge.dashed,
            Role::Edge,
        )?;
    }
    if edge.tip == Tip::Target {
        grid.arrow(target, arrow, ALONG_POS)?;
    } else {
        grid.wire(
            target,
            arrow,
            ALONG_POS | ALONG_NEG,
            edge.dashed,
            Role::Edge,
        )?;
    }
    grid.wire(target, to.along, ALONG_NEG, edge.dashed, Role::Node)?;
    if edge.label.is_empty() {
        return Some(());
    }
    if horizontal {
        // Transposed, "beside the line" is the free row under the target's box, then over it.
        let text = to.along + 2;
        if grid
            .label(target + 2, text, true, &edge.label, Role::Edge)
            .is_some()
        {
            return Some(());
        }
        if let Some(row) = target.checked_sub(2)
            && grid
                .label(row, text, true, &edge.label, Role::Edge)
                .is_some()
        {
            return Some(());
        }
        None
    } else {
        let text = to.along.saturating_sub(2);
        if grid
            .label(target + 2, text, false, &edge.label, Role::Edge)
            .is_some()
        {
            return Some(());
        }
        let width = UnicodeWidthStr::width(edge.label.as_str());
        if let Some(start) = target.checked_sub(1 + width)
            && grid
                .label(start, text, false, &edge.label, Role::Edge)
                .is_some()
        {
            return Some(());
        }
        None
    }
}

/// An edge that skips a layer, or points back at an earlier one: out of the source's side, along
/// the channel outside the canvas, back into the target's side. Every cell but the two borders
/// must be free, so a run across a box or another channel's line refuses the whole diagram.
fn channel_edge(
    grid: &mut Grid,
    from: Placed,
    to: Placed,
    edge: &Edge,
    channel: usize,
) -> Option<()> {
    // The layout has no room for a label beside a channel.
    if !edge.label.is_empty() {
        return None;
    }
    let out = (from.across_end(), from.mid_along());
    let entry = to.across_end();
    let arrow = (entry + 1, to.mid_along());
    let mut path = vec![out];
    for across in out.0 + 1..=channel {
        path.push((across, out.1));
    }
    if to.mid_along() > out.1 {
        for along in out.1 + 1..=to.mid_along() {
            path.push((channel, along));
        }
    } else {
        for along in (to.mid_along()..out.1).rev() {
            path.push((channel, along));
        }
    }
    for across in (arrow.0 + 1..channel).rev() {
        path.push((across, to.mid_along()));
    }
    for &(across, along) in &path[1..] {
        if !grid.free(across, along) {
            return None;
        }
    }
    if !grid.free(arrow.0, arrow.1) {
        return None;
    }
    grid.wire_path(&path, edge.dashed, Role::Edge)?;
    grid.wire(entry, to.mid_along(), ACROSS_POS, edge.dashed, Role::Node)?;
    grid.arrow(arrow.0, arrow.1, ACROSS_NEG)
}

/// One finished screen cell. `Blank` still emits a space (the vendor's filler), `Covered` is the
/// second cell of a wide character and emits nothing at all, `Ink` is a glyph.
#[derive(Clone, Copy)]
enum Screen {
    Blank,
    Covered,
    Ink(char, Role),
}

/// The finished picture: screen cells, then runs of one role, with trailing blanks dropped (as
/// the vendor's `finish` does).
fn spans(grid: &Grid, plan: &Plan) -> Vec<Vec<Span>> {
    let horizontal = plan.horizontal();
    let (width, height) = if horizontal {
        (plan.along, plan.across())
    } else {
        (plan.across(), plan.along)
    };
    let point = |across: usize, along: usize| match plan.flow {
        Flow::Down => (across, along),
        Flow::Up => (across, plan.along - 1 - along),
        Flow::Right => (along, across),
        Flow::Left => (plan.along - 1 - along, across),
    };
    let mut screen = vec![vec![Screen::Blank; width]; height];
    for across in 0..plan.across() {
        for along in 0..plan.along {
            let cell = grid.cell(across, along);
            if cell.arrow == 0 && cell.bits == 0 && cell.glyph.is_none() {
                continue;
            }
            let (x, y) = point(across, along);
            let glyph = match cell.glyph {
                Some(glyph) => glyph,
                None if cell.arrow != 0 => arrow_glyph(cell.arrow, plan.flow),
                None => wire_glyph(&cell, plan.flow),
            };
            screen[y][x] = Screen::Ink(glyph, cell.role.unwrap_or(Role::Edge));
        }
    }
    for run in &grid.text {
        let text_width = UnicodeWidthStr::width(run.text.as_str());
        let (start, end, y) = if run.runs_along {
            let (start, y) = point(run.across, run.along);
            let (end, _) = point(run.across, run.along + text_width - 1);
            (start, end, y)
        } else {
            let (start, y) = point(run.across, run.along);
            let (end, _) = point(run.across + text_width - 1, run.along);
            (start, end, y)
        };
        // The run's cells are read in reading order, so a mirrored axis (RL) starts at the
        // leftmost of the two.
        let mut x = start.min(end);
        for ch in run.text.chars() {
            let cells = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            screen[y][x] = Screen::Ink(ch, run.role);
            for cell in screen[y][x + 1..x + cells].iter_mut() {
                *cell = Screen::Covered;
            }
            x += cells;
        }
    }
    screen
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            for cell in row {
                let (ch, role) = match cell {
                    // A wide character's second cell carries nothing, like the vendor's `\0`.
                    Screen::Covered => continue,
                    Screen::Blank => (' ', Role::Edge),
                    Screen::Ink(ch, role) => (ch, role),
                };
                if let Some(last) = spans.last_mut()
                    && last.role == role
                {
                    last.text.push(ch);
                } else {
                    spans.push(Span {
                        text: ch.to_string(),
                        role,
                    });
                }
            }
            while let Some(last) = spans.last_mut() {
                last.text.truncate(last.text.trim_end().len());
                if !last.text.is_empty() {
                    break;
                }
                spans.pop();
            }
            spans
        })
        .collect()
}

/// The screen directions of the four layout-space wire directions, for this flow.
fn screen_bits(bits: u8, flow: Flow) -> u8 {
    // Screen bits: 1 up, 2 down, 4 left, 8 right.
    let (across_pos, across_neg) = match flow {
        Flow::Down | Flow::Up => (8, 4),
        Flow::Right | Flow::Left => (2, 1),
    };
    let (along_pos, along_neg) = match flow {
        Flow::Down => (2, 1),
        Flow::Up => (1, 2),
        Flow::Right => (8, 4),
        Flow::Left => (4, 8),
    };
    let mut screen = 0;
    if bits & ACROSS_POS != 0 {
        screen |= across_pos;
    }
    if bits & ACROSS_NEG != 0 {
        screen |= across_neg;
    }
    if bits & ALONG_POS != 0 {
        screen |= along_pos;
    }
    if bits & ALONG_NEG != 0 {
        screen |= along_neg;
    }
    screen
}

/// The rounded corner for one screen pair of directions: `┌`'s (right+down) is `╭`, `┐`'s
/// (left+down) is `╮`, `└`'s (up+right) is `╰` and `┘`'s (up+left) is `╯`.
fn round_glyph(bits: u8) -> Option<char> {
    match bits {
        10 => Some('╭'),
        6 => Some('╮'),
        9 => Some('╰'),
        5 => Some('╯'),
        _ => None,
    }
}

fn wire_glyph(cell: &Cell, flow: Flow) -> char {
    let dashed = cell.dashed == cell.bits;
    match screen_bits(cell.bits, flow) {
        0 => ' ',
        1..=3 => {
            if dashed {
                '┆'
            } else {
                '│'
            }
        }
        4 | 8 | 12 => {
            if dashed {
                '┄'
            } else {
                '─'
            }
        }
        5 => '┘',
        6 => '┐',
        7 => '┤',
        9 => '└',
        10 => '┌',
        11 => '├',
        13 => '┴',
        14 => '┬',
        15 => '┼',
        _ => ' ',
    }
}

/// An arrowhead pointing `direction` (a layout-space direction), on screen.
fn arrow_glyph(direction: u8, flow: Flow) -> char {
    match screen_bits(direction, flow) {
        1 => '▲',
        2 => '▼',
        4 => '◄',
        8 => '►',
        _ => ' ',
    }
}

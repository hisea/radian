//! Radian's entry point over the vendored grok-build Mermaid renderer (ADR 0042).
//!
//! [`mermaid`] is `crates/codegen/xai-grok-markdown/src/mermaid.rs` from
//! <https://github.com/xai-org/grok-build> at commit `4827113` (2026-09-15), vendored with two
//! recorded edits (see `RADIAN-CHANGES.md`): the two `use ratatui::…` lines become
//! `use crate::text::{Line, Modifier, Span, Style};` ([`text`] is Radian's shim), and the items
//! the dispatch below reuses are made `pub`. Everything else is byte-identical to upstream.
//!
//! [`render`] is Radian's one entry point. It is upstream's own 34-line dispatch
//! (`parse_graph` → `layout_flowchart`/`render_grouped`, then `parse_state`, `parse_class`,
//! `parse_er`, `parse_sequence`) **without** the trailing fallback: a source the renderer cannot
//! draw comes back as a [`Reason`], and the caller keeps the source (ADR 0028 R8). Upstream's
//! "open the image to view it in full" hint never appears in Radian.
//!
//! The vendored renderer caps at 128 nodes / 512 edges / 24 groups / depth 6 / 28-column labels
//! wrapped to 24 / 2 M cells of canvas; nodes and edges over a cap are dropped silently
//! (upstream `Graph::node_index`, ADR 0042 decision 7). A diagram that needs more columns than
//! the caller has is `TooWide`, not compressed or turned: Radian shows the source instead
//! (ADR 0042 decision 5).

pub mod text;

// The vendored file is byte-identical to upstream apart from the two `pub`/`use` edits listed
// in RADIAN-CHANGES.md; upstream's own `pub fn render` signature is formatted for a wider line
// than rustfmt would keep, and the fidelity check in `tests/pristine.rs` hashes the exact bytes.
// `#[rustfmt::skip]` keeps `cargo fmt --all -- --check` from reflowing them.
#[rustfmt::skip]
pub mod mermaid;

pub use text::{Line, Modifier, Span, Style};

use unicode_width::UnicodeWidthStr;

/// The three theme roles a rendered diagram is painted in (ADR 0029, ADR 0042 decision 4).
///
/// The vendored renderer's five roles collapse onto these: box drawing is [`Role::Node`], a
/// connector and its head are [`Role::Edge`], and node labels, edge labels and the (unused)
/// fallback title are [`Role::Text`]. `None` is a run with no role — leading spaces and the
/// like — and the caller paints it as plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Node,
    Edge,
    Text,
}

/// One rendered diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    /// The styled lines: each is a row of `(role, text)` runs, left to right.
    pub lines: Vec<Vec<(Option<Role>, String)>>,
    /// The same rows as plain text (trailing blanks trimmed), for measuring and for text-only
    /// callers. Its length is [`Art::height`].
    pub plain: Vec<String>,
    /// The widest [`Art::plain`] line, in display columns.
    pub width: usize,
    /// [`Art::plain`]'s number of lines.
    pub height: usize,
}

/// Why a source did not render (ADR 0042 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No supported diagram header, or a syntax the renderer does not know.
    Unsupported,
    /// The renderer draws it, but it needs `needed` columns and the caller offered fewer.
    TooWide { needed: usize },
    /// Over the renderer's canvas cap (`MAX_CANVAS_CELLS`).
    TooLarge,
}

/// Renders `source`, or explains why it cannot (ADR 0042 decision 3).
///
/// `max_width` is the caller's column budget (`None` = unbounded). A diagram that fits comes
/// back as an [`Art`]. On `TooWide`, the source is rendered again with no budget so `needed` is
/// the honest column count the drawing wants.
pub fn render(source: &str, max_width: Option<usize>) -> Result<Art, Reason> {
    if source.trim().is_empty() {
        return Err(Reason::Unsupported);
    }
    let styles = role_styles();
    match dispatch(source, &styles, max_width) {
        Dispatched::Rendered(art) => Ok(Art::from_mermaid(art)),
        Dispatched::Oversize(mermaid::Oversize::Width) => match dispatch(source, &styles, None) {
            Dispatched::Rendered(art) => Err(Reason::TooWide {
                needed: Art::from_mermaid(art).width,
            }),
            Dispatched::Oversize(mermaid::Oversize::Cells) => Err(Reason::TooLarge),
            // A width cap cannot be the reason a `None` render is too wide; keep the contract
            // explicit rather than fabricating a number.
            Dispatched::Oversize(mermaid::Oversize::Width) => Err(Reason::TooWide { needed: 0 }),
            Dispatched::Unsupported => Err(Reason::Unsupported),
        },
        Dispatched::Oversize(mermaid::Oversize::Cells) => Err(Reason::TooLarge),
        Dispatched::Unsupported => Err(Reason::Unsupported),
    }
}

/// The outcome of upstream's dispatch, before the fallback step that Radian omits.
enum Dispatched {
    Rendered(mermaid::MermaidArt),
    Oversize(mermaid::Oversize),
    Unsupported,
}

/// Upstream `render`'s dispatch (its lines 67-82), returning the reason instead of falling back.
fn dispatch(source: &str, styles: &mermaid::MermaidStyles, max_width: Option<usize>) -> Dispatched {
    let outcome: Option<Result<mermaid::MermaidArt, mermaid::Oversize>> =
        mermaid::parse_graph(source)
            .map(|graph| {
                if graph.groups.is_empty() {
                    mermaid::layout_flowchart(&graph, styles, max_width)
                } else {
                    mermaid::render_grouped(&graph, styles, max_width)
                }
            })
            .or_else(|| {
                mermaid::parse_state(source)
                    .map(|graph| mermaid::layout_flowchart(&graph, styles, max_width))
            })
            .or_else(|| {
                mermaid::parse_class(source)
                    .map(|(graph, infos)| mermaid::render_class(&graph, &infos, styles, max_width))
            })
            .or_else(|| {
                mermaid::parse_er(source)
                    .map(|(graph, infos)| mermaid::render_class(&graph, &infos, styles, max_width))
            })
            .or_else(|| {
                mermaid::parse_sequence(source)
                    .map(|seq| mermaid::layout_sequence(&seq, styles, max_width))
            });
    match outcome {
        Some(Ok(art)) => Dispatched::Rendered(art),
        Some(Err(oversize)) => Dispatched::Oversize(oversize),
        None => Dispatched::Unsupported,
    }
}

/// The five vendored roles as role-only styles: the renderer paints a role, the caller's theme
/// supplies the color (ADR 0029).
fn role_styles() -> mermaid::MermaidStyles {
    let role = |role: text::Role| Style(Some(role));
    mermaid::MermaidStyles {
        border: role(text::Role::Border),
        node_text: role(text::Role::NodeText),
        edge: role(text::Role::Edge),
        edge_label: role(text::Role::EdgeLabel),
        title: role(text::Role::Title),
    }
}

impl Art {
    /// Collapses a vendored [`mermaid::MermaidArt`] onto the three Radian roles.
    fn from_mermaid(art: mermaid::MermaidArt) -> Self {
        let width = art
            .plain_lines
            .iter()
            .map(|line| UnicodeWidthStr::width(line.as_str()))
            .max()
            .unwrap_or(0);
        let lines = art
            .styled_lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| (map_role(span.style.0), span.content.into_owned()))
                    .collect()
            })
            .collect();
        Self {
            lines,
            height: art.plain_lines.len(),
            plain: art.plain_lines,
            width,
        }
    }
}

/// The vendored five roles onto Radian's three (ADR 0042 decision 4).
fn map_role(role: Option<text::Role>) -> Option<Role> {
    match role {
        None => None,
        Some(text::Role::Border) => Some(Role::Node),
        Some(text::Role::Edge) => Some(Role::Edge),
        Some(text::Role::NodeText | text::Role::EdgeLabel | text::Role::Title) => Some(Role::Text),
    }
}

//! The minimal text shim the vendored `mermaid.rs` compiles against (ADR 0042 decision 2).
//!
//! Upstream `mermaid.rs` draws onto `ratatui::style::{Modifier, Style}` and
//! `ratatui::text::{Line, Span}`. Radian does not want the vendored crate to depend on ratatui
//! (both `radian-host` and `radian-tui` use it, and `radian-host` must stay ratatui-free), so
//! the two upstream `use` lines are rewritten to `use crate::text::{Line, Modifier, Span, Style}`
//! and this module supplies the four types with only the surface `mermaid.rs` calls.
//!
//! The types are deliberately not ratatui's:
//!
//! - [`Style`] is a **role**, not a color: `Style::default()` means "no role", and each other
//!   value names one of the diagram's five semantic roles. The real colors live in the caller's
//!   theme; `grok_mermaid::render` maps the roles down to the three theme roles (ADR 0029).
//! - [`Modifier`] and [`Style::add_modifier`] exist because upstream's `fallback` italicizes its
//!   "open the image" hint. Radian never calls `fallback`, but the vendored `render` does, so the
//!   method must exist; it keeps the role and drops the modifier (there is nothing to bold or
//!   slant in a role-only style).
//! - [`Span`] and [`Line`] are owned text plus a role, close enough to ratatui's shapes that the
//!   upstream code (and its 145 tests, which read `line.spans` and `span.content`) compile
//!   unchanged.

use std::borrow::Cow;

/// The five semantic roles a diagram is painted in (upstream `Cls`, plus the fallback title).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Node and frame box drawing.
    Border,
    /// A node's label text.
    NodeText,
    /// A connector line and its head.
    Edge,
    /// The text on a connector.
    EdgeLabel,
    /// The title inside the fallback frame (unused by Radian: it never calls `fallback`).
    Title,
}

/// A role, or no role at all. `Style::default()` is the no-role value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style(pub Option<Role>);

impl Style {
    /// Keeps the role and ignores the modifier: a role-only style has no weighting or slant.
    #[must_use]
    pub fn add_modifier(self, _modifier: Modifier) -> Self {
        self
    }
}

/// The one modifier upstream's `fallback` uses. Carried only so `add_modifier` can be called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifier;

impl Modifier {
    /// Upstream's `Modifier::ITALIC`.
    pub const ITALIC: Modifier = Modifier;
}

/// A run of text with one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span<'a> {
    pub content: Cow<'a, str>,
    pub style: Style,
}

impl<'a> Span<'a> {
    /// A styled run, mirroring `ratatui::text::Span::styled`.
    pub fn styled<S: Into<Cow<'a, str>>>(content: S, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }

    /// An unstyled run, mirroring `ratatui::text::Span::raw`.
    pub fn raw<S: Into<Cow<'a, str>>>(content: S) -> Self {
        Self {
            content: content.into(),
            style: Style::default(),
        }
    }
}

/// One display line: its runs, left to right.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line<'a> {
    pub spans: Vec<Span<'a>>,
}

impl<'a> From<Vec<Span<'a>>> for Line<'a> {
    fn from(spans: Vec<Span<'a>>) -> Self {
        Self { spans }
    }
}

impl<'a> From<Span<'a>> for Line<'a> {
    fn from(span: Span<'a>) -> Self {
        Self { spans: vec![span] }
    }
}

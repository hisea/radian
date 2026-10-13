//! A full-screen TUI shell on `radian-runtime` (Phase 4 / A).
//!
//! Full screen only (ADR 0009): the transcript, its own scrolling, and selection/copy are
//! ours to build. [`app::App`] is pure state, [`render`] turns it into a frame, and
//! [`terminal::TtyGuard`] owns the terminal's modified state.

pub mod app;
pub mod command;
pub mod picker;
pub mod profile;
pub mod render;
pub mod signals;
pub mod terminal;

pub use app::{App, ApprovalPrompt, Entry, EntryKind, Send, TurnState};

//! A full-screen TUI shell on `radian-runtime` (Phase 4 / A).
//!
//! Full screen only (ADR 0009): the transcript, its own scrolling, and selection/copy are
//! ours to build. [`app::App`] is pure state, [`render`] turns it into a frame, and
//! [`terminal::TtyGuard`] owns the terminal's modified state.

pub mod app;
pub mod command;
pub mod composer;
pub mod events;
pub mod find;
pub mod keys;
pub mod mention;
pub mod picker;
pub mod profile;
pub mod projection;
pub mod question;
pub mod render;
pub mod signals;
pub mod terminal;
pub mod theme;

pub use app::{
    App, Effect, Entry, EntryKind, Input, MemoryResult, PickerData, TurnState, UpdateResult,
};
pub use composer::Composer;

//! A full-screen TUI shell on `radian-runtime` (Phase 4 / A).
//!
//! Full screen only (ADR 0009): the transcript, its own scrolling, and selection/copy are
//! ours to build. [`app::App`] is pure state, [`render`] turns it into a frame, and
//! [`terminal::TtyGuard`] owns the terminal's modified state.

pub mod app;
pub mod blocks;
pub mod clip;
pub mod command;
pub mod composer;
pub mod events;
pub mod find;
pub mod keys;
pub mod login;
pub mod mention;
pub mod picker;
pub mod profile;
pub mod projection;
pub mod question;
pub mod render;
pub mod select;
pub mod signals;
pub mod terminal;
pub mod theme;
pub mod tools;
pub mod workspace;

pub use app::{
    App, Effect, Entry, EntryKind, Input, MemoryResult, MouseScrollDirection, PickerData, Startup,
    TurnState, UpdateResult,
};
pub use clip::{ClipboardMode, MAX_CLIPBOARD_BYTES};
pub use composer::Composer;
pub use select::{Anchor, Selection};

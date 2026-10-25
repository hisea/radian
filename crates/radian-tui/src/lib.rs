//! A full-screen TUI shell on `radian-runtime` (Phase 4 / A).
//!
//! Full screen only (ADR 0009): the transcript, its own scrolling, and selection/copy are
//! ours to build. [`app::App`] is pure state, [`render`] turns it into a frame, and
//! [`terminal::TtyGuard`] owns the terminal's modified state.

// The command entry point ([`cli`]) was the `radian` binary; it is a library module now so the
// `rad` alias can call the same code without a source file belonging to two build targets.
// `extern crate self` keeps its `radian_tui::…` paths resolving after the move.
extern crate self as radian_tui;

pub mod app;
pub mod arc;
pub mod attach;
pub mod bell;
pub mod blocks;
pub mod btw;
pub mod cli;
pub mod clip;
pub mod command;
pub mod composer;
pub mod copy;
pub mod events;
pub mod find;
pub mod footer;
pub mod highlight;
pub mod inspector;
pub mod keys;
pub mod login;
pub mod logout;
pub mod markdown;
pub mod media;
pub mod mention;
pub mod panel;
pub mod picker;
pub mod profile;
pub mod projection;
pub mod question;
pub mod render;
pub mod select;
pub mod select_list;
pub mod signals;
pub mod site_docs;
pub mod spinner;
pub mod startup;
pub mod terminal;
pub mod theme;
pub mod timing;
pub mod tools;
pub mod workspace;

pub use app::{
    App, Effect, Entry, EntryKind, Input, MemoryResult, MouseScrollDirection, PickerData, Startup,
    TurnState, UpdateResult,
};
pub use clip::{ClipboardMode, MAX_CLIPBOARD_BYTES};
pub use composer::Composer;
pub use select::{Anchor, Selection};
pub use startup::{StartupScreen, TIPS, tip_for};

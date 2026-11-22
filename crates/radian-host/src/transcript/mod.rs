//! The folded transcript (ADR 0023 U1).
//!
//! **One entry type, one fold.** A session's committed facts (the journal, read through
//! [`JournalObserver`](crate::observer::JournalObserver)) fold into a [`Vec<Entry>`](Entry).
//! Two front ends read the result: `radian attach` (a read-only journal view) and the TUI's
//! resume. Neither forks the folding rules, and the crate is ratatui-free, so a headless
//! reader needs no renderer.
//!
//! **Where the boundary sits.** This module owns what a transcript entry *is*, the message→entry
//! mapping, the tool card's data and its title text, and the plain text helpers (a message's display text, a `!` command block, a
//! row summary). `radian-tui` owns everything a screen needs: layout, themes, scrolling,
//! ratatui, the decoded images, and the live delta path. The TUI's `Entry` wraps the one here.
//!
//! **The JSON shape.** [`Entry`] serializes to one stable snake_case object with a `kind` tag;
//! the module's [`entry`] docs spell the object out, and `crates/radian-host/README.md` records
//! it.
//!
//! **The live path stays in `radian-tui`.** A running session's entries also carry state only a
//! screen has — the not-yet-committed streaming view, the reasoning clock, the compaction
//! numbers the `CompactionFinished` event supplied, the decoded images — so `App::push_message`
//! (live commits and resume) keeps its own entry bookkeeping. It is the same rules, not a second
//! interpretation: `radian-tui`'s `the_resume_mapping_and_the_shared_fold_agree_on_a_journal`
//! folds one journal through both and compares every entry.

pub mod attach;
pub mod entry;
pub mod parts;
pub mod tool;

pub use attach::{AttachFold, WORKING_LINE};
pub use entry::{Card, Change, CompactionInfo, Entry, EntryKind, EntryList, ImagePart};
pub use parts::{
    bash_context_command, bash_entry_from_context, display_parts, image_parts, text_of,
};
pub use tool::ToolCard;

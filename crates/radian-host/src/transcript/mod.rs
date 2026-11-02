//! The folded transcript (ADR 0023 U1).
//!
//! **One entry type, two folds.** A session's committed facts (the journal, read through
//! [`JournalObserver`](crate::observer::JournalObserver)) and an ACP attempt's `result.jsonl`
//! both fold into a [`Vec<Entry>`](Entry) of one type. Three front ends read the result:
//! `radian attach` (a read-only journal view), the arc TUI's conversation page (an attempt's
//! stream) and the hub (which sends the entries to a phone). None of them forks the folding
//! rules, and the crate is ratatui-free, so a headless reader needs no renderer.
//!
//! **Where the boundary sits.** This module owns what a transcript entry *is*, the message→entry
//! mapping, the tool card's data and its title text, the memory reference a settled call
//! resolves to, and the plain text helpers (a message's display text, a `!` command block, a
//! row summary). `radian-tui` owns everything a screen needs: layout, themes, scrolling,
//! ratatui, the decoded images, and the live delta path. The TUI's `Entry` wraps the one here.
//!
//! **The JSON shape.** [`Entry`] serializes to one stable snake_case object with a `kind` tag;
//! the module's [`entry`] docs spell the object out, and `crates/radian-host/README.md` records
//! it. The hub sends exactly this.
//!
//! **Two shapes the folds keep.** The journal fold writes a call's head as the tool renderer's
//! title (`shell $ make test`) and its open calls as `queued`/`running` notices; the ACP fold
//! writes the protocol's own title and status line. They are not unified: an ACP worker's stream
//! says something the journal does not, and each fold keeps the shape its source has.
//!
//! **The live path stays in `radian-tui`.** A running session's entries also carry state only a
//! screen has — the not-yet-committed streaming view, the reasoning clock, the compaction
//! numbers the `CompactionFinished` event supplied, the decoded images — so `App::push_message`
//! (live commits and resume) keeps its own entry bookkeeping. It is the same rules, not a second
//! interpretation: `radian-tui`'s `the_resume_mapping_and_the_shared_fold_agree_on_a_journal`
//! folds one journal through both and compares every entry.

pub mod acp;
pub mod attach;
pub mod entry;
pub mod memory;
pub mod parts;
pub mod tool;

pub use acp::AcpFold;
pub use attach::{AttachFold, WORKING_LINE};
pub use entry::{Card, Change, CompactionInfo, Entry, EntryKind, EntryList, ImagePart, origin_via};
pub use memory::{MemoryRecords, MemoryRef};
pub use parts::{
    SUMMARY_KEYS, bash_context_command, bash_entry_from_context, display_parts, first_summary,
    image_parts, line_breaks_to_spaces, text_of,
};
pub use tool::ToolCard;

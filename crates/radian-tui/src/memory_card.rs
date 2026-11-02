//! The memory card's data: a settled tool call that touched a memory record (ADR 0021, ADR 0035).
//!
//! The module moved to the host's folded transcript (`radian_host::transcript::memory`,
//! ADR 0023 U1): the fold resolves the reference, so the same journal produces the same
//! `ToolCard::memory` for the hub and for the TUI. This module keeps the path the TUI has always
//! imported from.

pub use radian_host::transcript::memory::{MemoryRecords, MemoryRef};

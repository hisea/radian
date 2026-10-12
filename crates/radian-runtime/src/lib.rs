//! The runtime layer above `agent-core` and `radian-session`: the durable inbox and (in a
//! later slice) the per-session driver.
//!
//! **Slice A status**: the four semantic input operations and their queue semantics are
//! implemented here ([`Inbox`]). The durable half — writing `InboxEnqueued` /
//! `InputClaimed` / `InputAdmitted` (and `TurnStarted` / `TurnFinished` / `ConfigChanged`)
//! to the session JSONL — is **blocked**: `radian-session` exposes no API to append runtime
//! events, and this slice must not change `radian-session`. See the crate `README.md` and
//! the report for the exact proposed API.

mod inbox;

pub use inbox::{CancelCause, Inbox, InboxEntry, InboxError, Lane};

//! The runtime layer above `agent-core` and `radian-session`: the durable inbox and (in a
//! later slice) the per-session driver.
//!
//! - [`Inbox`] holds the four semantic input operations and, when built with
//!   [`Inbox::durable`], writes `InboxEnqueued` before an input becomes visible.
//! - [`Driver`] owns one session: it applies configuration at turn boundaries, claims
//!   inputs, drives the core turn loop, and writes `InputClaimed`/`InputAdmitted`/
//!   `TurnStarted`/`TurnFinished`/`ConfigChanged` through `radian-session`.

mod config;
mod driver;
mod inbox;

pub use config::{ConfigQueue, ConfigSlot, ModelRoute};
pub use driver::{BudgetFor, CompactionSettingsFor};
pub use driver::{Driver, DriverError, RuntimeEvents};
pub use inbox::{CancelCause, Inbox, InboxEntry, InboxError, Lane};

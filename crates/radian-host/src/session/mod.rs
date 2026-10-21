//! The Session Facade (ADR 0023 U5): the UI's one handle on a session, and the in-process
//! implementation `open_session` builds for both binaries.
//!
//! The facade is a **door, not an owner**: the inbox, the driver, the journal, the tool
//! session and the approval routing belong to the runtime and the host. The UI holds an
//! `Arc<dyn SessionFacade>`, reads the projection through it, and sends commands; a command a
//! facade does not declare is refused, and `LocalSession` checks again when it runs one — the
//! host does not trust the UI's copy of the capability list.
//!
//! `open_session` is the one place that establishes a session: the journal, the permission and
//! memory hooks, the combined tools, the driver, the inbox and the projection. `radian-exec` and
//! `radian-tui` differ only in what they inject (their approver, audit sink, event sink,
//! runtime-event handler, wording and startup lines), which is [`HostBindings`].

mod facade;
mod ids;
mod local;
mod open;
mod setup_types;
mod support;

pub use facade::{DriverEnd, SessionError, SessionFacade};
pub use ids::{CommandRefused, SessionCapability, SessionCommand, SessionId, UserBashRecord};
pub use local::LocalSession;
pub use open::open_session;
pub use setup_types::{
    AccessBuilder, AccessParts, ApprovalRouting, HostBindings, SessionFacts, SessionQueue,
    SessionSetup,
};
pub use support::{resumed_access, resumed_model_route, session_hooks};

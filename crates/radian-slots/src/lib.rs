//! Runtime hot-configuration slots: type-erased, swappable model and policy seams.
//!
//! `agent-core` stays generic (`Agent<M, P, H>`). This crate supplies concrete `M`/`H`
//! adapters that a runtime can swap at run time without changing those generics or
//! introducing a global trait object. See `docs/adr/0004-hot-config-slots.md`.

mod config;
mod model_slot;
mod pin;
mod policy_slot;

pub use config::{ConfigChange, ConfigChanged, ConfigLog};
pub use model_slot::{DynModel, ModelSlot};
pub use pin::{PinnedModelSlot, with_run};
pub use policy_slot::{AllowAll, DenyAll, DynPolicy, Policy, PolicySlot};

//! A swappable sandbox handle (ADR 0038 slice 2, the `/access` switch).
//!
//! The shell and background-job tools take the `Arc<Sandbox>` out of the slot when a call
//! begins and hold it for the whole call. A tier switch therefore takes effect on the
//! **next** call, and a command already running keeps the sandbox it started with
//! (ADR 0038 decision 2: "a command already running is unaffected").
//!
//! The mechanism is a `std::sync::RwLock` over an `Option<Arc<Sandbox>>`, not an
//! `arc-swap`: the lock is held only long enough to clone the `Arc` (never across an
//! `await`), the repository already uses `std::sync` locks for exactly this, and a new
//! dependency for a microsecond lock is not worth it. A poisoned lock still returns the
//! last value rather than panicking, matching [`crate::Sandbox`]'s callers.

use std::sync::{Arc, RwLock};

use crate::{Sandbox, SandboxMode};

/// The sandbox in force for a tool session, replaceable at run time.
///
/// `Clone` shares the same slot (an `Arc` inside), so the host can keep one handle to switch
/// while the coding tools and the TUI's `!`/`!!` path read the same value.
#[derive(Clone, Default)]
pub struct SandboxSlot {
    inner: Arc<RwLock<Option<Arc<Sandbox>>>>,
}

impl SandboxSlot {
    /// A slot with no sandbox: the `safe` tier, or a host whose tools never run a command.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A slot holding `sandbox`.
    pub fn with(sandbox: Sandbox) -> Self {
        Self::from_option(Some(sandbox))
    }

    /// A slot from an optional sandbox (`None` is the `safe` tier's `build_sandbox` result).
    pub fn from_option(sandbox: Option<Sandbox>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(sandbox.map(Arc::new))),
        }
    }

    /// The sandbox in force right now. A caller that holds it across an `await` (every
    /// shell/job call) clones the `Arc` here and never touches the slot again for that call.
    pub fn get(&self) -> Option<Arc<Sandbox>> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the sandbox. `None` is the `safe` tier (no shell). The next call reads the
    /// new value; a call already in flight keeps the `Arc` it cloned.
    pub fn set(&self, sandbox: Option<Sandbox>) {
        *self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = sandbox.map(Arc::new);
    }

    /// The current mode, for `shell`'s advertised specs.
    pub fn mode(&self) -> Option<SandboxMode> {
        self.get().map(|sandbox| sandbox.mode())
    }

    /// The current backend name, for a startup/`/access` description.
    pub fn backend_name(&self) -> Option<&'static str> {
        self.get().map(|sandbox| sandbox.backend_name())
    }

    /// Whether the slot currently holds a sandbox (the `!`/`!!` availability check).
    pub fn is_some(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinct workspace directory per label. `FullAccess` is transparent (no probe), so
    /// these tests never pay the enforced backend's probe cost.
    fn workspace(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radian-slot-test-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn sandbox(label: &str) -> Sandbox {
        Sandbox::new(SandboxMode::FullAccess, workspace(label)).expect("full-access needs no probe")
    }

    /// The slot starts empty and reads back what was set.
    #[test]
    fn the_slot_holds_and_replaces_a_sandbox() {
        let slot = SandboxSlot::empty();
        assert!(slot.get().is_none());
        assert_eq!(slot.mode(), None);

        slot.set(Some(sandbox("hold")));
        assert_eq!(slot.mode(), Some(SandboxMode::FullAccess));
        assert_eq!(slot.backend_name(), Some("none"));
        assert!(slot.is_some());

        slot.set(None);
        assert!(slot.get().is_none());
        assert!(!slot.is_some());
    }

    /// A clone shares the same slot, so a host handle switches what the tools read.
    #[test]
    fn a_clone_shares_the_slot() {
        let slot = SandboxSlot::empty();
        let reader = slot.clone();
        slot.set(Some(sandbox("clone")));
        assert_eq!(reader.mode(), Some(SandboxMode::FullAccess));
    }

    /// A caller that cloned the `Arc` before a `set` keeps the old sandbox: this is the
    /// "a command already running keeps the sandbox it started with" rule (ADR 0038).
    #[test]
    fn a_held_arc_does_not_change_when_the_slot_is_replaced() {
        let slot = SandboxSlot::with(sandbox("held"));
        let held = slot.get().expect("the first sandbox");
        slot.set(Some(sandbox("next")));
        assert_eq!(held.policy().workspace, workspace("held"));
        assert_eq!(
            slot.get().expect("the next sandbox").policy().workspace,
            workspace("next")
        );
    }
}

//! The process-level **heavy-command slot gate** (ADR 0030 O8, revision 1 R4).
//!
//! Limiting how many agents run at once (`max_tasks`) does not limit CPU: several agents each
//! running `cargo build` at the same time is what pushed load average into the hundreds during
//! orchestration. This module is a **process-level** semaphore ([`HeavySlots`], default
//! [`DEFAULT_HEAVY_SLOTS`] = 2) that a shell command which matches a **heavy pattern**
//! (default [`DEFAULT_HEAVY_PATTERNS`]) must hold while it runs. Every session the host runs —
//! the lead's and every task's — draws on the same process gate, so N sessions share 2 slots.
//!
//! # What it is, and is not
//!
//! - It is **best effort** (ADR 0030 R4): the match is a prefix of the *normalised* command
//!   string, so `bash -c "cargo test"`, a script, or an alias is invisible and takes no slot.
//!   The gate shapes load; it is not a security boundary.
//! - It gates **shell** commands (foreground and background, ADR 0037). A background job holds
//!   its slot for the job's whole life and drops it on settle; `job_kill` still stops the job
//!   (it signals the job's session and never consults the gate), so a held slot cannot block a
//!   kill.
//! - The slice 9 **verify runner** will call [`HeavySlots::acquire`] directly, on the same
//!   process gate, so a task's verify commands queue behind its shell builds. That runner does
//!   not exist yet; the hook is this method.
//!
//! # The process gate
//!
//! Production never injects a gate: [`CodingTools`](crate::CodingTools) resolves
//! [`HeavySlots::process`] at session open, and [`HeavySlots::set_process`] installs the
//! configured one. First configuration wins and is read once, matching ADR 0030 slice 9
//! ("the config is read once, at startup"); a test injects its own with
//! [`CodingTools::with_heavy`](crate::CodingTools::with_heavy), which never touches the process
//! gate.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use agent_core::CancellationToken;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How many heavy commands may run at once in one process (ADR 0030 O8).
pub const DEFAULT_HEAVY_SLOTS: usize = 2;

/// The shell commands that count as heavy by default (ADR 0030 O8 / revision 1 R4): a prefix
/// match on the normalised command.
pub const DEFAULT_HEAVY_PATTERNS: [&str; 3] = ["cargo build", "cargo test", "cargo clippy"];

/// The installed process gate. `OnceLock`, because the orchestration config is read once per
/// process (ADR 0030 slice 9); the first [`HeavySlots::set_process`] wins.
static PROCESS: OnceLock<Arc<HeavySlots>> = OnceLock::new();

/// The process-level heavy-command gate.
pub struct HeavySlots {
    /// `Arc` so a permit can be **owned** and outlive the acquire call (a background job holds
    /// its slot for its whole life).
    permits: Arc<Semaphore>,
    capacity: usize,
    /// Normalised patterns; a command is heavy when one is a word-boundary prefix of it.
    patterns: Vec<String>,
}

/// A held heavy slot. Dropping it frees the slot; [`HeavyPermit::waited`] is how long the
/// call queued for it, so the shell tool can say it was queued.
#[derive(Debug)]
pub struct HeavyPermit {
    _permit: OwnedSemaphorePermit,
    waited: Duration,
}

impl HeavyPermit {
    /// How long the call waited for a slot before running.
    pub fn waited(&self) -> Duration {
        self.waited
    }
}

/// An acquire was cancelled while waiting (the turn was cancelled). The command never ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeavyCancelled;

impl HeavySlots {
    /// Builds a gate with `slots` concurrent slots (at least 1) and `patterns`. Empty patterns
    /// mean nothing is heavy.
    pub fn new<P>(slots: usize, patterns: impl IntoIterator<Item = P>) -> Arc<Self>
    where
        P: Into<String>,
    {
        let capacity = slots.max(1);
        let patterns = patterns
            .into_iter()
            .map(|pattern| normalize(pattern.into().as_str()))
            .filter(|pattern| !pattern.is_empty())
            .collect();
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
            patterns,
        })
    }

    /// The installed process gate, or the ADR defaults when none was installed. Install one
    /// with [`HeavySlots::set_process`] before any shell runs.
    pub fn process() -> Arc<Self> {
        Arc::clone(
            PROCESS.get_or_init(|| HeavySlots::new(DEFAULT_HEAVY_SLOTS, DEFAULT_HEAVY_PATTERNS)),
        )
    }

    /// Installs the process gate from configuration. The first call wins (`true`); a later call
    /// is ignored (`false`) because the orchestration config is read once (ADR 0030 slice 9).
    pub fn set_process<P>(slots: usize, patterns: impl IntoIterator<Item = P>) -> bool
    where
        P: Into<String>,
    {
        PROCESS.set(HeavySlots::new(slots, patterns)).is_ok()
    }

    /// The configured slot count.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The normalised heavy patterns, in the order configured.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// The slots free right now (a test/observability read).
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// Whether `command` is heavy: a normalised pattern is a word-boundary prefix of the
    /// normalised command (ADR 0030 R4's best-effort match).
    pub fn is_heavy(&self, command: &str) -> bool {
        let command = normalize(command);
        self.patterns
            .iter()
            .any(|pattern| prefix_at_word_boundary(&command, pattern))
    }

    /// Waits for a heavy slot if `command` is heavy. `Ok(None)` means the command is not heavy
    /// and runs ungated; `Ok(Some(permit))` means a slot is held and must live as long as the
    /// command; `Err(HeavyCancelled)` means the wait was cancelled and the command must not run.
    pub async fn acquire(
        &self,
        command: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<HeavyPermit>, HeavyCancelled> {
        if !self.is_heavy(command) {
            return Ok(None);
        }
        let started = Instant::now();
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HeavyCancelled),
            // The gate is never closed; an `Err` here is a dropped semaphore, which from the
            // caller's point of view is the same as a cancellation: do not run the command.
            permit = Arc::clone(&self.permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return Err(HeavyCancelled),
            },
        };
        Ok(Some(HeavyPermit {
            _permit: permit,
            waited: started.elapsed(),
        }))
    }
}

/// The normalised form a heavy pattern is compared against: whitespace runs collapse to one
/// space and the ends are trimmed, so `cargo   test` matches `cargo test`.
fn normalize(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `command` begins with `pattern` at a word boundary: the pattern is the whole command
/// or is followed by a character that is not part of a command word (`-`, `_`, `.`, or a
/// letter/digit). So `cargo test` matches `cargo test --lib`, `cargo test;` and `cargo test`,
/// but not `cargo testkit`.
fn prefix_at_word_boundary(command: &str, pattern: &str) -> bool {
    let Some(rest) = command.strip_prefix(pattern) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(next) => !(next.is_alphanumeric() || matches!(next, '-' | '_' | '.')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_patterns_match_their_commands_and_only_theirs() {
        let gate = HeavySlots::new(DEFAULT_HEAVY_SLOTS, DEFAULT_HEAVY_PATTERNS);
        assert!(gate.is_heavy("cargo test"));
        assert!(gate.is_heavy("cargo test --locked --no-fail-fast"));
        assert!(gate.is_heavy("  cargo   build  --release "));
        assert!(gate.is_heavy("cargo clippy --workspace"));
        assert!(gate.is_heavy("cargo test;"));
        assert!(gate.is_heavy("cargo test && echo done"));
        // Not heavy.
        assert!(!gate.is_heavy("cargo testkit"));
        assert!(!gate.is_heavy("cargo build-script"));
        assert!(!gate.is_heavy("cargo fmt"));
        assert!(!gate.is_heavy("echo cargo test"));
        assert!(!gate.is_heavy("bash -c 'cargo test'"));
    }

    #[tokio::test]
    async fn a_heavy_command_waits_for_a_slot_and_a_light_one_does_not() {
        let gate = HeavySlots::new(1, ["cargo test"]);
        let cancel = CancellationToken::new();
        let held = gate
            .acquire("cargo test", &cancel)
            .await
            .expect("acquire")
            .expect("heavy");
        // The light command takes no permit even though the only slot is held.
        assert!(
            gate.acquire("echo hi", &cancel)
                .await
                .expect("light")
                .is_none()
        );
        assert_eq!(gate.available(), 0);
        drop(held);
        assert_eq!(gate.available(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_wait_returns_cancelled_and_never_acquires() {
        let gate = HeavySlots::new(1, ["cargo test"]);
        let cancel = CancellationToken::new();
        let _held = gate
            .acquire("cargo test", &cancel)
            .await
            .expect("acquire")
            .expect("heavy");
        let waiting = CancellationToken::new();
        let token = waiting.clone();
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire("cargo test", &token).await })
        };
        // Cancel while it waits; it must return `Err`, not a permit.
        waiting.cancel();
        let outcome = waiter.await.expect("the task");
        assert!(matches!(outcome, Err(HeavyCancelled)));
        assert_eq!(
            gate.available(),
            0,
            "no slot was handed to the cancelled wait"
        );
    }
}

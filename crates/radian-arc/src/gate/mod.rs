//! The gate: profiles, stage checkouts, verdicts and the integration candidate
//! (ADR 0048 decisions 6, 10 and 11).
//!
//! A gate is a fixed profile: a list of stages with no conditions and no dynamic graph. Each
//! stage runs once against the verification candidate and writes one verdict. A brief can add
//! stages; it can never remove a required one.
//!
//! - [`GateConfig`] / [`Profile`] / [`Stage`] — the `[gate]` table of `config.toml`.
//! - [`GateCheckout`] — the detached linked worktree of one stage under
//!   `<repository>/.radian/arc/gates/<stage>/`, fetches from the remote name it is given.
//! - [`ExclusiveLock`] — the lock `commands` and `mutation` hold for a whole stage. The
//!   integration stage holds its own `gates/integration.lock`, so two runs never overlap.
//! - [`CommandRun`] — the `commands` stage, on the candidate `verified_sha` merged onto main.
//! - [`ReviewRun`] — the read-only `review` session on the same candidate.
//! - [`background`] — the gate as a separate process a pass starts and does not wait for.
//! - [`Verdict`] and [`verdict_set_digest`] — the records and the digest of an approval.
//! - [`IntegrationRun`] and [`merge_ready`] — the merge candidate and the main check.
//!
//! Git runs through the crate's private helper (`crate::git`), which always passes
//! `-c core.hooksPath=`, sets `GIT_CONFIG_NOSYSTEM=1` and clears every inherited `GIT_*`
//! variable that could point a command at another repository, so a repository hook never
//! runs for a gate operation and a caller's environment never redirects it.

mod checkout;
mod commands;
mod config;
mod error;
mod exclusive;
mod git;
mod integration;
mod review;
mod stage;
mod verdict;

pub mod background;

pub use checkout::{GateCheckout, gates_dir};
pub use commands::CommandRun;
pub use config::{
    DEFAULT_COMMAND_TIMEOUT_SECS, DEFAULT_REVIEW_TIMEOUT_SECS, GateConfig, LISTED_STAGES, Profile,
    ReviewConfig, STAGE_NAMES, Stage, StageSpec, effective_stages,
};
pub use error::GateError;
pub use exclusive::ExclusiveLock;
pub use integration::{IntegrationRun, merge_ready};
pub use review::{OWNED_EXEC_ARGS, REVIEW_RUNS, REVIEW_TOOLS, ReviewRun, owned_name, parse_report};
pub use verdict::{
    CommandRecord, ReviewReport, VERDICT_SCHEMA_VERSION, Verdict, VerdictResult, attempt_dir,
    now_utc, required_passed, valid_verdicts, verdict_path, verdict_set_digest,
};

//! arc: the plan and task orchestrator (ADR 0048).
//!
//! Everything arc keeps for one repository is in `<repository>/.radian/arc/`. The crate has
//! these modules:
//!
//! - [`store`]: the plans, tasks, ledger and locks, with the subcommands `radian arc init`,
//!   `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `pause`, `start`, `cancel`,
//!   `reset-budget`, `merge`, `reject`.
//! - [`gate`]: a fixed sequence of stages against one commit, the separate checkout for each
//!   stage, the `commands` stage, verdicts, the integration candidate, and the background
//!   gate process a scheduling pass starts.
//! - [`review`]: verification, the owner's approval and the merge, and reject.
//! - [`attempt`]: one attempt's claim, wrapper child, applied result, cancel and heartbeat
//!   liveness.
//! - [`run`]: `radian arc run`, the scheduling loop that advances every auto level.
//! - [`work`]: `radian arc work`, the trusted wrapper around one worker attempt. It is
//!   internal: arc runs it, a person does not.
//! - [`acp`]: a minimal synchronous Agent Client Protocol v1 client over stdio, independent of
//!   the agent it starts. The wrapper uses it for a flow's `acp` worker.
//! - [`worker_args`]: the worker's `radian exec` command line, parsed with the `radian exec`
//!   parser before `start`, a `run` pass or `confirm` acts.
//! - [`mcp`]: the MCP server the lead writes plans and tasks through (`radian arc mcp`).
//! - [`read`]: the read-only views the arc TUI draws (the remote main, attempts and their
//!   verdicts, a task's diff, and the merge page's stage rows, review and commits). Nothing
//!   in it writes or fetches.
//!
//! Every git call of these modules is built by the private `git` module.
//!
//! The arc TUI lives in `radian-tui`; it reads through [`read`].
pub mod acp;
pub mod attempt;
pub mod gate;
mod git;
pub mod mcp;
mod process;
pub mod read;
pub mod review;
pub mod run;
pub mod store;
#[cfg(test)]
mod test_support;
pub mod work;
pub mod worker_args;

/// Not part of the public API: the integration tests drive arc's locked fetch (ADR 0048
/// decision 10) through this, because the `git` module stays private to the crate. `Ok(())`
/// is a fetch that exited zero; any other outcome is the line a stage would fail with.
#[doc(hidden)]
pub fn fetch_for_test(
    dir: &std::path::Path,
    args: &[&str],
    limit: std::time::Duration,
) -> Result<(), String> {
    git::fetch_for_test(dir, args, limit)
}

/// Not part of the public API: the lock file the locked fetch uses in `dir`'s repository, so a
/// test can hold it and prove two worktrees share it.
#[doc(hidden)]
pub fn fetch_lock_for_test(dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    git::fetch_lock_path(dir).map_err(|error| error.to_string())
}

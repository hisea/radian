//! arc: the plan and task orchestrator (ADR 0048).
//!
//! Everything arc keeps for one repository is in `<repository>/.radian/arc/`. The crate has
//! these modules:
//!
//! - [`store`]: the plans, tasks, ledger and locks, with the subcommands `radian arc init`,
//!   `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `pause`, `start`, `cancel`,
//!   `reset-budget`, `merge`, `reject`.
//! - [`gate`]: a fixed sequence of stages against one commit, the separate checkout for each
//!   stage, the `commands` stage, verdicts, and the integration candidate.
//! - [`review`]: verification, the owner's approval and the merge, and reject.
//! - [`work`]: `radian arc work`, the trusted wrapper around one worker attempt. It is
//!   internal: arc runs it, a person does not.
//! - [`mcp`]: the MCP server the lead writes plans and tasks through (`radian arc mcp`).
//!
//! Every git call of these modules is built by the private `git` module.
//!
//! The scheduling loop and the TUI are later slices.
pub mod attempt;
pub mod gate;
mod git;
pub mod mcp;
mod process;
pub mod review;
pub mod store;
pub mod work;

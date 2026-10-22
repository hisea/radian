//! arc: the plan and task orchestrator (ADR 0048).
//!
//! Everything arc keeps for one repository is in `<repository>/.radian/arc/`. The crate has
//! three modules:
//!
//! - [`store`]: the plans, tasks, ledger and locks, with the subcommands `radian arc init`,
//!   `add`, `list`, `show` and `adopt`.
//! - [`gate`]: a fixed sequence of stages against one commit, the separate checkout for each
//!   stage, the `commands` stage, verdicts, and the integration candidate.
//! - [`work`]: `radian arc work`, the trusted wrapper around one worker attempt. It is
//!   internal: arc runs it, a person does not.
//!
//! Every git call of the three modules is built by the private `git` module.
//!
//! The scheduling loop, merge, and the TUI are later slices.
pub mod gate;
mod git;
pub mod store;
pub mod work;

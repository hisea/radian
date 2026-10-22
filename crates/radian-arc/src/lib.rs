//! arc: the plan and task orchestrator (ADR 0048).
//!
//! Everything arc keeps for one repository is in `<repository>/.radian/arc/`; [`store`]
//! owns those files, the locks, the ledger, and the transactions that change them, and the
//! subcommands `radian arc init`, `add`, `list`, `show` and `adopt`.
//!
//! [`gate`] is the gate: a fixed sequence of stages against one commit. It owns the
//! `[gate]` configuration shape, the separate checkout for each stage, the `commands`
//! stage, the verdict record and its digest, and the integration candidate. It has no CLI
//! and does not schedule anything.
//!
//! The scheduling loop, attempts, merge, and the TUI are later slices.
pub mod gate;
pub mod store;

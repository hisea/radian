//! arc: the plan and task orchestrator (ADR 0048).
//!
//! This build has the storage layer and the subcommands that read and write it:
//! `radian arc init`, `add`, `list`, `show`, and `adopt`. The scheduling loop, attempts,
//! the gate, merge, and the TUI are later slices.
//!
//! Everything arc keeps for one repository is in `<repository>/.radian/arc/`; [`store`]
//! owns those files, the locks, the ledger, and the transactions that change them.
pub mod store;

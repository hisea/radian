//! arc: the plan and task orchestrator (ADR 0048).
//!
//! This build has the storage layer, the subcommands that read and write it (`radian arc
//! init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `pause`), and the MCP server
//! the lead writes plans and tasks through (`radian arc mcp`). The scheduling loop,
//! attempts, the gate, merge, and the TUI are later slices.
//!
//! Everything arc keeps for one repository is in `<repository>/.radian/arc/`; [`store`]
//! owns those files, the locks, the ledger, and the transactions that change them, and
//! [`mcp`] exposes the lead's tools over those same transactions.
pub mod mcp;
pub mod store;

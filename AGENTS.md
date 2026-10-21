# radian agent instructions

This repository holds a minimal headless Rust agent runtime and the coding agent built on it. `agent-core` is the runtime (turn loop, tools, hooks, steering, the journal seam); `agent-extensions` adds MCP stdio tools; `radian-session`, `radian-runtime`, `radian-permissions`, `radian-sandbox`, `radian-coding`, `radian-memory` and `radian-host` are the layers around it (durable sessions, the inbox and driver, allow/ask/deny, the OS sandbox, the built-in tools, durable memory, host plumbing); the two provider adapters are checked by the `agent-provider-contract` suite; and `agent-cli` and `radian-tui` are the headless command (`radian exec`) and the `radian` binary (`rad` is an alias). The crate-by-crate table with each one's docs is `README.md`. The design is in `docs/minimal-headless-rust-agent-runtime-design.md`. Read this file before you change code.

## Decisions are in `docs/adr/`

Every decision that constrains the code lives in a numbered ADR there, newest number last. Read the ones that touch what you are changing **before** you change it, and do not contradict one: journal ordering and failure semantics (0001), tool execution markers and recovery (0002), the interrupted streaming prefix (0003), hot configuration slots (0004), host message metadata and transcript replacement (0005), the session JSONL format (0006), usage (0007), config schema and migration (0008), fullscreen-only TUI (0009), durable versus live events (0010), inbox semantics (0011), permission sources (0012), no telemetry by default (0013), reasoning content (0014), context budget estimation (0015), the tier names and the `workspace` tier's network and devices (0016), model and hook errors end the turn (0017), the model catalog and auto compaction (0018), the transcript replacement commit (0019), the `~/.radian/` layout and what the sandbox may read (0020), memory (0021), session fork (0022), the TUI architecture and composition boundary (0023), structured user interaction (0024), skills (0025), provider credentials and model switching (0026), the TUI chrome, footer and extension contributions (0027), transcript rich content (0028), theme files (0029), project trust (0031), one binary: `radian exec` replaces `agent` (0032), the `!` and `!!` shell commands (0033), reporting agent state to herdr (0034), the TUI visual language (0035), the base system prompt and context files such as AGENTS.md (0036), background commands and completion notices (0037), access switching and `.git` writes (0038), host file access hardening (0039), journal group commit (0040), the Mermaid renderer (0042), and single-session radian: one general session, a one-shot `subagent` tool, no sandbox by default with the sandbox saved per project, herdr first-class (0043). Permission rules and what the shell matcher cannot see are in `crates/radian-permissions/README.md`. The TUI's hand-test list and its panic-path precondition are in `crates/radian-tui/README.md`. The sandbox backend's real limits are in `docs/sandbox.md`. A change that needs a decision reversed needs the ADR changed first, in its own docs-only commit, not a quiet edit.

An ADR is a snapshot of the decisions in force. It holds no revision history, no change table, no dated amendments; git holds the history. Intended work lives in `docs/backlog/`, one file per item: a backlog item references the ADRs it touches, never the reverse (a backlog file is deleted when done, and the ADR stays a clean snapshot). When an item lands, the same branch changes the ADR in its own docs-only commit and deletes the backlog file; all of it merges to `main` together with the code. To change a decision, edit the ADR so it states the new decision as if it had always been the decision, in a docs-only commit on the implementing branch, merged with the code. Never change a decision quietly inside a code commit.

## Verify

Run these before you say a change is done. All of them must pass. The `journal-selfcheck` runs replay every session's journal against its transcript, so they check every transcript write. The minimum Rust version is 1.99.1.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p agent-core --locked --features journal-selfcheck
cargo clippy -p agent-core --all-targets --features journal-selfcheck -- -D warnings
bash docs/schema/check-fixtures.sh
bash docs/schema/check-config-fixtures.sh
```

Run tests with `--no-fail-fast`. Cargo stops at the first failing test binary by
default, which makes a mutation look like it was caught by one test when it was
caught by seven, and hides failures in the binaries that never ran.

A change to a provider adapter must also keep **both** contract suites green:

```sh
cargo test -p agent-provider-openai-compatible --features testing --test contract
cargo test -p agent-provider-anthropic --features testing --test contract
```

See `crates/agent-provider-contract/README.md`, including what the contract still assumes about a
provider and the two escape hatches: a harness declares what its provider has no concept
of, and the contract then asserts the absence. Never invent a wire value a provider does
not send to make the contract pass.

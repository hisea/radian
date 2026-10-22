# radian agent instructions

This repository holds a minimal headless Rust agent runtime and the coding agent built on it. `radian-core` is the runtime (turn loop, tools, hooks, steering, the journal seam); `agent-extensions` adds MCP stdio tools; `radian-session`, `radian-runtime`, `radian-permissions`, `radian-sandbox`, `radian-coding`, `radian-memory` and `radian-host` are the layers around it (durable sessions, the inbox and driver, allow/ask/deny, the OS sandbox, the built-in tools, durable memory, host plumbing); the two provider adapters are checked by the `agent-provider-contract` suite; and `radian-exec` and `radian-tui` are the headless command (`radian exec`) and the `radian` binary (`rad` is an alias). The crate-by-crate table with each one's docs is `README.md`. The design is in `ARCHITECTURE.md`. Read this file before you change code.

## Decisions are in `docs/adr/`

Every decision that constrains the code lives in a numbered ADR there, newest number last. Read the ones that touch what you are changing **before** you change it, and do not contradict one: journal ordering and failure semantics (0001), tool execution markers and recovery (0002), the interrupted streaming prefix (0003), hot configuration slots (0004), host message metadata and transcript replacement (0005), the session JSONL format (0006), usage (0007), config schema and migration (0008), fullscreen-only TUI (0009), durable versus live events (0010), inbox semantics (0011), permission sources (0012), no telemetry by default (0013), reasoning content (0014), context budget estimation (0015), the tier names and the `workspace` tier's network and devices (0016), model and hook errors end the turn (0017), the model catalog and auto compaction (0018), the transcript replacement commit (0019), the `~/.radian/` layout and what the sandbox may read (0020), memory (0021), session fork (0022), the TUI architecture and composition boundary (0023), structured user interaction (0024), skills (0025), provider credentials and model switching (0026), the TUI chrome, footer and extension contributions (0027), transcript rich content (0028), theme files (0029), project trust (0031), one binary: `radian exec` replaces `agent` (0032), the `!` and `!!` shell commands (0033), reporting agent state to herdr (0034), the TUI visual language (0035), the base system prompt and context files such as AGENTS.md (0036), background commands and completion notices (0037), access switching and `.git` writes (0038), host file access hardening (0039), journal group commit (0040), the Mermaid renderer (0042), and single-session radian: one general session, a one-shot `subagent` tool, no sandbox by default with the sandbox saved per project, herdr first-class (0043), and arc, the plan and task orchestrator that runs each task in a separate `radian exec` worker (0048). Permission rules and what the shell matcher cannot see are in `crates/radian-permissions/README.md`. The TUI's hand-test list and its panic-path precondition are in `crates/radian-tui/README.md`. The sandbox backend's real limits are in `crates/radian-sandbox/LIMITS.md`. A change that needs a decision reversed needs the ADR changed first, in its own docs-only commit, not a quiet edit.

An ADR is a snapshot of the decisions in force. It holds no revision history, no change table, no dated amendments; git holds the history. Intended work lives in `docs/backlog/`, one file per item: a backlog item references the ADRs it touches, never the reverse (a backlog file is deleted when done, and the ADR stays a clean snapshot). When an item lands, the same branch changes the ADR in its own docs-only commit and deletes the backlog file; all of it merges to `main` together with the code. To change a decision, edit the ADR so it states the new decision as if it had always been the decision, in a docs-only commit on the implementing branch, merged with the code. Never change a decision quietly inside a code commit.

## Document every input that changes behavior

The user manual in `docs/user/` is compiled into the binary, and it is the only reference a user has. If a change adds, removes or changes an input that changes radian's behavior, the same branch updates the manual. An input is anything a user can set or place: a flag (a hidden one too), a configuration key, an environment variable, a file whose presence or content changes behavior, a frontmatter field, a field that a hook or a footer `command` item returns, a theme key, a slash command argument, a key binding. A key that is accepted but has no effect is documented as inert. Two kinds of input are not documented: an input that exists only for tests, and an internal input that radian passes to its own child processes (for example the hidden `radian exec` flags `--no-config-mcp`, `--parent`, `--parent-seq` and `--tools`, which the `subagent` tool sets). Each such input says so in its code comment.

Each entry gives the name, the type, the default from the code, every allowed value of a closed set (never "for example" in place of the list), limits and units, what it does, and when it takes effect. A nested structure is documented at every level; a built-in preset is shown in full. One fact lives on one page, and other pages link to it. The docs drift tests read names, value sets and defaults from the code. A new closed set gets one list that the parser and the test both read.

## Verify

Run these before you say a change is done. All of them must pass. The `journal-selfcheck` runs replay every session's journal against its transcript, so they check every transcript write. The minimum Rust version is 1.99.1.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --locked --no-fail-fast
cargo nextest run -p radian-core --locked --features journal-selfcheck --no-fail-fast
cargo clippy -p radian-core --all-targets --features journal-selfcheck -- -D warnings
bash docs/schema/check-fixtures.sh
bash docs/schema/check-config-fixtures.sh
```

`scripts/verify.sh` runs exactly that list, in that order, after the two binary builds (`radian` and `mcp-fixture`) and before the three provider contract suites; a docs drift test reads this block and the script's `run_step` lines and fails when they differ.

Run the suite with `--no-fail-fast`: nextest runs every test binary's tests as
separate processes in parallel (`.config/nextest.toml` caps the pty suite's
concurrency and kills a hung test by timeout), so one failure must not hide the
rest. Plain `cargo test` keeps working for a contributor without nextest: no test
depends on nextest to pass, only the pty concurrency cap lives in the nextest
config. The pty binary under plain `cargo test` runs at libtest's default thread
count; on a loaded machine that can flake, so
`cargo test -p radian-tui --test pty -- --test-threads=3` approximates the old
in-process gate (see `crates/radian-tui/README.md`).

A change to a provider adapter must also keep **both** contract suites green:

```sh
cargo test -p agent-provider-openai-compatible --features testing --test contract
cargo test -p agent-provider-anthropic --features testing --test contract
```

See `crates/agent-provider-contract/README.md`, including what the contract still assumes about a
provider and the two escape hatches: a harness declares what its provider has no concept
of, and the contract then asserts the absence. Never invent a wire value a provider does
not send to make the contract pass.

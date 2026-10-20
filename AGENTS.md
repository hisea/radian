# radian agent instructions

This repository holds a minimal headless Rust agent runtime and the coding agent built on it. `agent-core` is the runtime (turn loop, tools, hooks, steering, the journal seam); `agent-extensions` adds MCP stdio tools; `radian-session`, `radian-runtime`, `radian-permissions`, `radian-sandbox`, `radian-coding`, `radian-memory` and `radian-host` are the layers around it (durable sessions, the inbox and driver, allow/ask/deny, the OS sandbox, the built-in tools, durable memory, host plumbing); the two provider adapters are checked by the `agent-provider-contract` suite; and `agent-cli` and `radian-tui` are the headless command (`radian exec`) and the `radian` binary (`rad` is an alias). The crate-by-crate table with each one's docs is `README.md`. The design is in `docs/minimal-headless-rust-agent-runtime-design.md`. Read this file before you change code.

## Decisions are in `docs/adr/`

Every decision that constrains the code lives in a numbered ADR there, newest number last. Read the ones that touch what you are changing **before** you change it, and do not contradict one: journal ordering and failure semantics (0001), tool execution markers and recovery (0002), the interrupted streaming prefix (0003), hot configuration slots (0004), host message metadata and transcript replacement (0005), the session JSONL format (0006), usage (0007), config schema and migration (0008), fullscreen-only TUI (0009), durable versus live events (0010), inbox semantics (0011), permission sources (0012), no telemetry by default (0013), reasoning content (0014), context budget estimation (0015), the tier names and the `workspace` tier's network and devices (0016), model and hook errors end the turn (0017), the model catalog and auto compaction (0018), the transcript replacement commit (0019), the `~/.radian/` layout and what the sandbox may read (0020), memory (0021), session fork (0022), the TUI architecture and composition boundary (0023), structured user interaction (0024), skills (0025), provider credentials and model switching (0026), the TUI chrome, footer and extension contributions (0027), transcript rich content (0028), theme files (0029), project trust (0031), one binary: `radian exec` replaces `agent` (0032), the `!` and `!!` shell commands (0033), reporting agent state to herdr (0034), the TUI visual language (0035), the base system prompt and context files such as AGENTS.md (0036), background commands and completion notices (0037), access switching and `.git` writes (0038), host file access hardening (0039), journal group commit (0040), the Mermaid renderer (0042), and single-session radian: one general session, a one-shot `subagent` tool, no sandbox by default with the sandbox saved per project, herdr first-class (0043). Permission rules and what the shell matcher cannot see are in `crates/radian-permissions/README.md`. The TUI's hand-test list and its panic-path precondition are in `crates/radian-tui/README.md`. The sandbox backend's real limits are in `docs/sandbox.md`. A change that needs a decision reversed needs the ADR changed first, in its own docs-only commit, not a quiet edit.

An ADR is a snapshot of the decisions in force. It holds no revision history, no change table, no dated amendments; git holds the history. To change a decision, edit the ADR so it states the new decision as if it had always been the decision, in a docs-only commit whose message says what changed and why. Never change a decision quietly inside a code commit.

## Verify

Run these before you say a change is done. All of them must pass. The `journal-selfcheck` runs replay every session's journal against its transcript, so they check every transcript write. The minimum Rust version is 1.88.

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

The workspace MSRV is checked, not assumed: `cargo +1.88 check --workspace
--all-targets` passes (`rustup toolchain install 1.88 --profile minimal`).

A change to a provider adapter must also keep **both** contract suites green:

```sh
cargo test -p agent-provider-openai-compatible --features testing --test contract
cargo test -p agent-provider-anthropic --features testing --test contract
```

See `docs/provider-contract.md`, including what the contract still assumes about a
provider and the two escape hatches: a harness declares what its provider has no concept
of, and the contract then asserts the absence. Never invent a wire value a provider does
not send to make the contract pass.

## Rules for delegating to pi

Use this section when the owner asks you to hand work to pi in herdr. pi is a second coding agent in another pane. It is cheaper than you, and it does not know what you know.

### Pick the task

- Delegate a task when its result can be checked by commands and tests, and it does not depend on timing. Examples: benchmarks, adding tests with known expected results, mechanical refactors, syncing docs with code.
- Do not hand off a task that depends on concurrency, ordering, cancellation, process lifetime, or the inner behavior of a dependency, unless you have already investigated it. Do these yourself, or put the verified facts in the brief first.
- Do not hand off code that you cannot test on this machine, such as Windows process control.

### Before you write the brief

- Read the code the task touches. Read the source of every dependency whose behavior the task depends on. For example, read the service loop of `rmcp` before you ask for anything about its notifications.
- Write only what you verified as fact. Mark each claim you did not verify as unverified. A wrong claim in a brief costs a whole round of work.
- Check that what you ask for is possible. Do not ask for a deterministic proof of an order that the dependency does not guarantee.
- Split a test that covers two causes into two tests, for example with a parameter. A test that mixes them fails for the wrong reason.

### Set up

- Check `test "$HERDR_ENV" = 1`. Find pi with `herdr agent list`. Read its pane with `herdr pane read <pane>` before you send anything. Make sure its input box is empty: a stale draft is added to the front of your message.
- Give each task its own git worktree and branch under the gitignored `.radian-worktrees/` (human and agent worktrees use it): `git worktree add .radian-worktrees/<task> -b pi/<task> origin/main`. pi works only there. A session in the main checkout never touches `.radian-worktrees/`. pi commits on the branch and does not push or merge. You merge after the review, and only when the owner agrees. Remove the worktree after the merge.
- Write the brief in a file outside the repository, in the scratchpad. Send one line with `herdr agent prompt <pane> "<one line that names the file>"`. Do not send long or multi-line text.

### What the brief holds

- The worktree path and the branch, and a ban on touching the main working tree.
- The files to read first, the background, and the steps.
- The limits: do not change the public API of `agent-core` without saying why, no new dependencies without a reason, minimum Rust 1.88, follow the style around it.
- The acceptance commands from the Verify section, and how many times to repeat flaky-prone tests.
- The write-as-you-go rule: pi writes each finished piece to disk and commits it before it starts the next piece. It keeps a short progress note in the worktree (what is done, what is verified, what is next) and updates it after each step. A context compaction loses work that lives only in the conversation; it does not lose a file or a commit.
- The test rule: each new test must fail when the feature is broken. pi runs the test against the broken code, and does a mutation check for each behavior it adds. The report names the test that caught each mutation.
- The report: files changed, the design in a few lines, the command results, what pi is unsure about, and the commit id. pi must say when it is unsure, and must not guess.
- The notice: when pi finishes or is blocked, it runs `herdr agent prompt <your pane> "<one line>"`. Use your own pane id from `$HERDR_PANE_ID`. The line has the status, the commit id, whether all the commands passed, and what you must decide.

### While pi works

- Do not poll. The notice wakes you.
- Do not change pi's worktree while pi works.
- If pi seems stuck, read its pane. Signs are a rising token count with no edits, and the same question asked again and again. Interrupt with `herdr agent send-keys <pane> esc`, then send one message with the correct facts and a clear decision. An interrupt loses only the current reasoning.
- Keep a watch on the cost counter in the pane.

### Review

- Do not trust the report. Say that something is verified only after you ran it yourself.
- Check out the commit in a separate worktree under the scratchpad. Run the Verify commands there. Run the tests that can be flaky five times.
- Do a mutation check of pi's new tests in that worktree: break the code, see that a test fails, restore the code. A test that stays green is a hole. Tell pi about it.
- Read the diff. Read each comment that claims a guarantee, such as "race-free", and check the claim. Look for a test that is named for one thing and checks another.
- Send the problems back in a new brief on the same branch, as a new commit. Do not amend.

### Shell hygiene

- Test fixtures start child processes, and the children keep the stderr of the test run. An orphan keeps `cargo test 2>&1 | grep` blocked until the orphan ends. Write the output to a file. Find the orphan by the pid that the fixture logged, and kill only that pid.
- Do not use `sleep` to wait. Do not leave a changed source file behind after a mutation check: restore it from a copy and compare.

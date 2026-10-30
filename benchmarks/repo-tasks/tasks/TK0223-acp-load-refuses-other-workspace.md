---
id: TK0223
slug: acp-load-refuses-other-workspace
base: 8777e666c7573ecab334cf908cadd01bfe06c1ad
outcome: fail
crates: radian-exec, radian-host, radian-coding
---
## Brief

## Problem
`crates/radian-exec/src/acp/mod.rs`, `open_session_request` with `load = Some(id)`: it checks the request `cwd` against `--workspace` (or adopts it), then resumes the named journal. It never compares that workspace with the journal's `SessionCreated.data.workspace`. A session created in workspace A can be loaded by a process whose workspace is B; the conversation then continues with tools rooted in B while its history refers to A's files. The host already refuses this elsewhere: `crates/radian-host/src/sessions.rs:161` ("session … is not in this workspace; only the current workspace can be …") for export, and listing only shows the current workspace's sessions.

## What to change
- Before opening the session for `session/load`, read the journal's `SessionCreated` workspace and refuse with JSON-RPC `INVALID_PARAMS` when it is not the process workspace. Reuse the host's existing check (the `sessions.rs` helper, or the same comparison it uses, including how it canonicalizes paths and symlinks such as macOS `/var` → `/private/var`) rather than writing a second rule.
- Check what `radian --resume` and `radian exec --resume` do with a session from another workspace, and match them; say in the final answer what they do.
- A refused load leaves the process able to accept a later valid `session/load` or `session/new` (state is not consumed).

## Edge cases to cover
1. Session created in A, `radian acp --workspace B`, `session/load` with `cwd` B → refused, message names the session and says it is not in this workspace; no journal write.
2. Same, `--workspace` absent, `cwd` B → refused.
3. Same session, process workspace A reached through a symlinked path → accepted.
4. A refused load followed by a valid `session/load` of a session in B → succeeds.
5. A journal without a readable `SessionCreated` (truncated file) → the existing error path, not a panic.

## Docs
`docs/user/acp.md` (the `session/load` bullet): state the workspace rule. ADR 0049 if it lists the load checks (docs-only commit first). Mutation rows in `crates/radian-exec/MUTATIONS.md`.

## How you work in arc

You are an arc worker (ADR 0048 §7). arc gave you this brief on stdin, runs you in the `workspace` sandbox, and checks your result after you exit. Nobody reads a pane or a notice: everything the lead learns comes from your branch and your final answer.

- **Branch:** your worktree is already on `work/<this task's id>`, cut from `origin/main`. Commit there. Do not switch branches, rebase, push, or touch any other branch; the wrapper pushes after you exit. If a rework asks you to take main, use `git merge --no-ff --no-edit origin/main` (the repository sets `merge.ff=only`).
- **Commit, and end with a clean tree:** the wrapper fails the attempt when `git status` shows any uncommitted or untracked file, even when the work itself is done. Commit everything that belongs to the change; put scratch files only under `.radian/` (git ignores it; create it with `mkdir -p .radian` first) and nowhere else. Run `git status --short` as your last command before your final answer and make sure it prints nothing.
- **Never change `.radian/arc/`:** a commit that touches it fails the attempt. You cannot read it either; that is expected.
- **Commits:** whole pieces. An ADR change is its own docs-only commit, first. Code and its tests go together. The manual may go with the code or in its own docs-only commit. In a `MUTATIONS.md`, add a new section at the end of the file with one blank line before its `##` heading.
- **Read source with `read`/`grep`/`glob`** (they take `limit`, `offset`, `max_matches`, `context`; call several in one reply), not `cat`/`sed`/`grep`/`tail` in shell. Shell is for commands and test logs. ADR files are under `docs/adr/` with their full names; use `glob docs/adr/00NN-*` instead of guessing a name.
- **Tests:** first `cargo build -p radian-tui --bin radian -p agent-extensions --bin mcp-fixture` (the integration tests need both binaries). Use the worktree's own target directory; never set `CARGO_TARGET_DIR`. Run nextest with `--profile sandbox` (it skips the tests that fail only inside the workspace sandbox). While you iterate, run only the test binary you are changing with `--test <binary>` (add `-E 'test(<name>)'` to narrow it further): `-p <crate>` with only `-E` still compiles every test binary of the crate, which in `radian-tui` is three times slower. Run each crate the brief names in full once at the end. In a rework, run only the test binaries your change touches; the gate runs the whole crates. Do not run `git fetch`: the sandbox cannot reach the remote, and arc fetched `origin/main` before your attempt started. Never run `--workspace`: it exceeds the tool time limit and fails on sandbox-only tests; the gate runs it. Write test output to a file under `.radian/` (`cargo nextest run -p <crate> --locked --profile sandbox --no-fail-fast > .radian/t1.log 2>&1; echo exit=$?`), never through a pipe. Run `cargo fmt --all` before each commit. The arc gate runs fmt, clippy, the touched crates and `radian-tui`, a read-only review at the same time, then the full Verify of your branch merged onto main; a red gate comes back to you as a rework brief.
- **Review items in a rework:** for each item, first write a test from the trigger the item names and see it fail, then fix the code until it passes, then check the other places the item's reasoning applies to (the same field or function used elsewhere, the other layout, the other caller). An item fixed in one place but not its twin comes back in the next review. If an item is wrong, do not change the code for it; say why in your final answer.
- **Mutations:** for each mutation the brief lists, break the code once by hand, run the tests, name the test that fails, restore from a backup copy under `.radian/` (never `git checkout -- <file>` over uncommitted work). Mutation ids are the task id followed by `-M<n>` (see "Mutation records" in `AGENTS.md`).
- **Rules:** read `AGENTS.md` and the ADRs the brief names first. Never contradict an ADR beyond the change the brief orders. Do not loosen an existing check, gate, deny rule or drift test to make a test pass. Do not set process-wide environment variables in tests. Every input that changes behaviour is documented in `docs/user/`, key bindings included. A new environment variable read with `env::var`/`var_os` needs a row in `docs/user/environment.md`, or, when only tests set it, an entry in `TEST_ONLY` in `crates/radian-host/tests/env_docs.rs`; run `cargo nextest run -p radian-host --test env_docs --profile sandbox` before your last commit (the full Verify runs it and fails otherwise).
- **Edge cases:** when the brief has an `Edge cases to cover` section, write one test per case from its trigger, see it fail on the old code where the old code had the defect, and name each test in your final answer. A case you could not cover goes in your final answer with the reason.
- **The manual says what the code does:** before your last commit, `grep` `docs/user/`, `docs/adr/` and the crate READMEs for every name your change adds or changes (flag, key, field, state, file, command, function the docs mention) and for the behaviour it changes. Read each sentence that matches and fix every one the change made false, including pages the brief does not name. The drift tests only check names and values, not what a sentence says.
- **Sandbox:** if the sandbox blocks a step, do not work around it and do not change the sandbox. Note the exact command and error and continue with what you can.
- **Stop instead of guessing:** if the task needs a decision the brief does not make, commit what is sound and end with the question.

## Your final answer

Your last message is the report the lead reads. Give, briefly:
1. the commits (short sha + subject);
2. the tests you added and that each failed on the old code;
3. each mutation and the test that caught it;
4. any decision you made that the brief did not;
5. sandbox friction (exact command and error), or "none";
6. open questions, or "none";
7. the doc sentences you checked for the manual rule (`path:line`, and whether you changed each), or "no doc mentions".
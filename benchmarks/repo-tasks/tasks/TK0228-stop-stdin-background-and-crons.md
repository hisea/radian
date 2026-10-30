---
id: TK0228
slug: stop-stdin-background-and-crons
base: 80d74766f83ed82016871c721d9ba5d792940ac9
outcome: fail
crates: radian-coding, radian-host
---
## Brief

## Problem
A `Stop` hook gets only `hook_event_name`, `cwd`, `session_id` and `last_assistant_message` (`crates/radian-host/src/hooks.rs`, `run_stop_once`, around line 344). A script cannot tell "really done" from "waiting on a background job", and cannot tell whether this turn was itself opened by a Stop continuation.

## Decisions
Read `docs/backlog/hooks-stop-and-feedback.md` §1; it is the spec. In short, add three always-present fields to the `Stop` stdin, Claude Code names:
- `stop_hook_active` (bool): true when this turn was opened by a Stop continuation (hook exit 2, or the built-in intent-stop validator; later tasks add `decision: "block"` and `additionalContext`, so put the flag where any continuation sets it). Set when `CommandHooks` sends the continuation followup; cleared after the continued turn's `Stop` ran, and by any user message (same place as `reset_wakes`, `session/local.rs` SendFollowup/Steer/Inject).
- `background_tasks` (array): running jobs from `Jobs::list()` (`crates/radian-coding/src/jobs.rs`), each `{ "id", "type": "shell", "status": "running", "description": <first line of the command>, "command": <full command> }`; `[]` when none. `CommandHooks` already holds a `Weak` to the registry (`set_jobs`).
- `session_crons` (array): `{ "id": "loop", "type": "loop" }` while `/loop` is active (`loop_active`); `{ "id": "goal", "type": "goal", "status": "active" | "paused" }` while the goal is Active or Paused; `[]` otherwise.

## Edge cases to cover
1. No jobs, no loop, no goal → `background_tasks: []`, `session_crons: []`, `stop_hook_active: false` (fields present, not omitted).
2. A background job running at turn end → one entry with the right id and full multi-line `command`, `description` is only the first line.
3. Hook exits 2 → the continued turn's Stop stdin has `stop_hook_active: true`; the turn after that (no continuation) has `false`.
4. A continuation is pending and the user sends a message (followup, steer or inject) → the next Stop has `stop_hook_active: false`.
5. A non-Completed turn (cancelled) after a continuation → the flag is cleared; the next Completed turn's Stop has `false`.
6. Goal paused → `session_crons` has the goal entry with `status: "paused"`; goal completed or cleared → no entry.
7. The jobs registry was dropped (the `Weak` fails to upgrade) → `background_tasks: []`, no panic.

## Docs
`docs/user/hooks.md`: each field with name, type, default and when it is filled. ADR 0045 §2 stdin list, in a docs-only commit. Mutation rows in `crates/radian-host/MUTATIONS.md`.

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
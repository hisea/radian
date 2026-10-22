# radian-arc

arc is the plan and task orchestrator (ADR 0048). It is part of the `radian` binary; it is
not a separate program. This crate is its control plane. Today it holds one module.

## `work`: the trusted worker wrapper

`radian arc work --task <id> --attempt <n> --token <token> [--resume] [--allow-noop]
[-- <extra radian exec args>]` runs one attempt of one task, the brief on stdin. It is an
**internal** subcommand: arc runs it, a person does not. It is hidden from `--help` and has
no manual page.

One run:

1. Creates or reuses the linked worktree `<repository>/.radian/arc/worktrees/<task>` on
   branch `work/<task>`. A first attempt starts from `origin/main` and records `base_sha`
   in `<repository>/.radian/arc/attempts/<task>/base_sha`; a later attempt reuses that
   base. A flock on the worktree directory keeps two attempts of one task apart (a second
   wrapper exits 4).
2. Writes the host-local attempt record `attempts/<task>/<attempt>/identity.json` (token,
   the worker's pid and process start time, the session id, the worktree, the branch, the
   start time).
3. Runs the current executable as `radian exec -p - --session-id <task> --workspace
   <worktree> --profile workspace --yes --output jsonl` (a rework attempt passes
   `--resume <task>` instead of `--session-id`), with the caller's extra arguments and one
   hidden `--deny-read <path>` per entry of `.radian/arc/` except `worktrees/`. Its stdout
   is `runs/<task>/attempts/<attempt>/result.jsonl`, its stderr `stderr.log`; the attempt
   fields go to `execution.json`.
4. After the child exits, reads the last `Result` line and the exit code, reads
   `head_sha`, lists uncommitted and untracked paths, and checks: the branch exists,
   `head_sha` descends from `base_sha`, the worktree HEAD is on the branch, the tree is
   clean, and the delivery check (a branch with no change past the base fails unless
   `--allow-noop`; any changed path under `.radian/arc/` fails).
5. Pushes `work/<task>` only when the outcome is `completed` and every check passed, then
   verifies the remote ref with `git ls-remote`.
6. Prints one `AttemptFinished` JSON line as its last stdout line (and writes it to
   `runs/<task>/attempts/<attempt>/attempt_finished.json` and
   `attempts/<task>/<attempt>/status.json`):

   ```json
   {"type": "AttemptFinished", "attempt_id": "<task>-<n>", "token": "…",
    "exit_code": 0, "outcome": "completed", "base_sha": "…", "head_sha": "…",
    "dirty": [], "pushed": true, "remote_sha": "…", "failure": null}
   ```

   `outcome` is the child's `Result.outcome`, or `terminated` when the child exited
   without a `Result`.

7. Exits with the child's exit code (ADR 0044 §3). A check that fails after a `completed`
   child exits 1, with the reason in `failure`. A second attempt of the same task exits 4.

Every git command the wrapper runs carries `-c core.hooksPath=` (empty),
`GIT_CONFIG_NOSYSTEM=1` and `GIT_TERMINAL_PROMPT=0`, so no file the model can write changes
what the wrapper runs and an unattended push never waits for a prompt. The push credential
is in the wrapper's environment only; in v0 the wrapper and the model are the same OS user,
so that is not isolation.

The `workspace` tier is required: unattended work never runs in `full`. The worker's sandbox
denies reading every entry of `.radian/arc/` except `worktrees/`, so the worker cannot read
plans, tasks, the ledger, the runs or another attempt's records.

## Not here yet

`init`, the backlog, the ledger, the gate, the scheduling loop, retries, stall timers,
cancel, reconcile, the arc TUI and `radian arc mcp`. This slice is Spike 0: the wrapper and
the questions it answers about the `workspace` sandbox.

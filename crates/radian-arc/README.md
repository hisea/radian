# radian-arc

arc is the plan and task orchestrator (ADR 0048). It is part of the `radian` binary; it is
not a separate program. This crate is its control plane. Today it holds one module.

## `work`: the trusted worker wrapper

`radian arc work --task <id> --attempt <n> --token <token> [--resume] [--allow-noop]
[--remote <name>] [--main-branch <name>] [-- <extra radian exec args>]` runs one attempt
of one task, the brief on stdin. It is an **internal** subcommand: arc runs it, a person does
not. It is hidden from `--help` and has no manual page. `--remote` (default `origin`) and
`--main-branch` (default `main`) are internal too: arc sets them from the repository
configuration.

One run:

1. Creates or reuses the linked worktree `<repository>/.radian/arc/worktrees/<task>` on
   branch `work/<task>`. A first attempt fetches `<remote> <main-branch>` and starts from the
   fetched tip, recording `base_sha` in `<repository>/.radian/arc/attempts/<task>/base_sha`;
   a later attempt reuses that base. A flock on the worktree directory keeps two attempts of
   one task apart (a second wrapper exits 4).
2. Before a reused worktree's child runs, checks that `work/<task>` still points at the tip
   the previous attempt recorded in `attempts/<task>/last_head` and that the worktree HEAD
   is still on that branch. A ref that moved outside this task (another worker can move it
   from its `workspace` sandbox) fails the attempt with that reason before any child runs;
   nothing is pushed. Every attempt that ran ends by recording its branch tip there again.
3. Writes the host-local attempt record `attempts/<task>/<attempt>/identity.json` (token,
   the worker's pid and process start time, the session id, the worktree, the branch, the
   start time).
4. Runs the current executable as `radian exec -p - --session-id <task> --workspace
   <worktree> --profile workspace --yes --output jsonl` (a rework attempt passes
   `--resume <task>` instead of `--session-id`), with the caller's extra arguments and one
   hidden `--deny-read <path>` per entry of `.radian/arc/` except `worktrees/`. Its stdout
   is `runs/<task>/attempts/<attempt>/result.jsonl`, its stderr `stderr.log`; the attempt
   fields go to `execution.json`.
5. After the child exits, reads the last `Result` line and the exit code, reads
   `head_sha`, lists uncommitted and untracked paths, and checks: the branch exists,
   `head_sha` descends from `base_sha` and from the tip the attempt started on, the worktree
   HEAD is on the branch, the tree is clean, and the delivery check (a branch with no change
   past the base fails unless `--allow-noop`; any changed path under `.radian/arc/` fails).
6. Pushes `work/<task>` to the configured remote only when the outcome is `completed` and
   every check passed, then verifies the remote ref with `git ls-remote`.
6. Prints one `AttemptFinished` JSON line as its last stdout line (and writes it to
   `runs/<task>/attempts/<attempt>/attempt_finished.json` and
   `attempts/<task>/<attempt>/status.json`):

   ```json
   {"type": "AttemptFinished", "attempt_id": "<task>-<n>", "token": "…",
    "exit_code": 0, "outcome": "completed", "base_sha": "…", "head_sha": "…",
    "dirty": [], "pushed": true, "remote_sha": "…", "failure": null}
   ```

   `outcome` is the child's `Result.outcome`, or `terminated` when the child exited
   without a `Result`. A start refusal (the moved-ref check) also ends with
   `outcome: "terminated"`, `exit_code: 1` and its reason in `failure`; it runs no child
   and does not update `last_head`.

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

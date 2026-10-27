---
description: The arc scheduling loop: one pass, its output and loop.log, serve, and its signals.
---

# arc: the loop

This page covers `radian arc run` and `radian arc serve`; the other `radian arc` commands are in [`arc`](index.md), the gate and its failures are in [`arc/gate`](gate.md), the TUI is in [`arc/tui`](tui.md), and the files arc keeps are in [`arc/files`](files.md).

## `radian arc run`

`run` is one pass of the scheduling loop without an interface; [`radian arc serve`](#radian-arc-serve) repeats it.
It takes `runner.lock`, makes every transition the auto levels allow now, saves the state, and exits.
A pass only schedules: it starts a worker or a gate, collects what one left, and moves the state, so a pass ends in seconds.
Waiting for you, a `retry_at` in the future, a running worker, or a running gate does not keep it running.
A task of a plan at `auto` `off` is never started or merged by `run`; its verification still advances.
A task of a `cancelled` plan is never claimed, gated or merged, and a cancelled plan is not listed in the summary.

The arc TUI never runs this pass itself: when no loop holds the lock it starts `radian arc serve` as a detached process (see [`arc/tui`](tui.md#the-loop)). A TUI and a loop (`run` or `serve`) exclude each other through `runner.lock`: a second loop that cannot get it exits `4`.
Quitting the TUI never stops a loop, whether the TUI started it or not.

Before its first pass, and before each pass claims a task, it checks the worker's `radian exec` line the way `start` does: a rejected line exits `3`, claims nothing, and changes no file (see [`worker.exec_args`](files.md#configtoml)).

`run` takes no option. The old form `radian arc run --watch`, with or without `--interval`, is refused: it is now `radian arc serve`. It exits `2` with one stderr line, ``radian arc: `run --watch` is now `radian arc serve` ``.

One pass repeats until a pass changes nothing, in this order:

1. reconcile every task in `doing` (see [Reconcile](gate.md#reconcile)): apply an `AttemptFinished` that is there, leave a live attempt alone, apply the retry rules to a dead one. A `doing.reconciling` task of a `cancelled` plan is ended at `failed.cancelled` after the same process check `cancel` makes;
2. move every `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`; this is also how a worker model wait ends (see [A worker wait on an unavailable model](gate.md#a-worker-wait-on-an-unavailable-model));
3. re-verify, by itself, a task a `commands` or integration command failure blocked, once main moved (see [A command failure and a moved main](gate.md#a-command-failure-and-a-moved-main)), and a task waiting on an unavailable review model once its `retry_at` has passed (see [A review wait on an unavailable model](gate.md#a-review-wait-on-an-unavailable-model)): claim a new attempt without a worker. The two guards of the moved-main path apply to it, and the review wait uses no count; neither is a retry: they count toward neither `max_attempts` nor `auto_rework`;
4. for every task in `review.verifying` (every plan, including a task that a moved branch or a changed digest sent back, and one step 3 just claimed): apply the verdicts a background gate left, or start its gate in the background;
5. for each confirmed plan at `auto` `run` or `merge`, claim every `todo.ready` task whose `after` tasks are merged, within its flow's [`flows.<name>.concurrency`](files.md#configtoml) and up to [`limits.concurrency`](files.md#configtoml) attempts in flight, and start each wrapper without waiting for it. A ready task whose flow is full is skipped, so a task of another flow further down the claim order is claimed in the same pass;
6. for each plan at `auto` `merge`, approve a verified task and start its integration in the background, or finish an integration a gate left.
   `run` writes the same `human` pass verdict `radian arc merge` writes, with `approver` `auto` in it.

Steps 4, 5 and 6 walk the backlog in *claim order*: the plan's `priority` (`P0` first), then the plan id from small to large, then the task's order in the plan and its `after` list.
So the higher priority's task is verified, claimed, merged, and queued for a stage's lock before a lower priority's.
`priority` only decides which plan gets a free concurrency slot: a running attempt is never preempted, so a changed priority takes effect at the next free slot.

The long work runs outside the pass: a worker is a separate `radian arc work` process with its own limits and record, and a gate is a separate process with its own record.
A stage this build does not run, or a branch that moved, is reported in the summary; the gate's refusal is recorded, and `run` starts no new gate for that attempt until the inputs change (for verification the branch comes back; for integration a new `head_sha`, a new main or a changed `config.toml`; or `start`/`reverify` claims a new attempt).
A gate whose process exited without writing any record is reported as `<task> verify cannot run: its gate exited without a record` (or `<task> integration cannot run: …`), recorded for that attempt; no new gate starts while its inputs stay the same, and `show` prints the same text as the `rework_reason` (see [the gate run](gate.md#the-gate-run)).
A start is counted exactly once: a gate whose record is `running`, or `started` with no inputs, and whose gate lock is free is one death, and it is started again; after [`limits.max_gate_deaths`](files.md#configtoml) deaths for the same attempt and inputs the record becomes `died`, the summary prints `<task> verify cannot run: its gate died after it started`, `show` prints the same text as the `rework_reason`, and no gate starts while the inputs stay the same. A gate the loop stopped itself is `interrupted` instead, and the next loop starts it again without counting. A gate whose record is `started` with the inputs it was planned with is the one start that is not a death: it is reported as `no_record` above.
The next pass applies a wrapper's early failure (a refused argument, a held worktree lock) the same way, so its concurrency slot is free at once (see [Reconcile](gate.md#reconcile)).

A one-shot `run` starts its work and exits without waiting for it; the next `run` collects the result.
Use [`radian arc serve`](#radian-arc-serve) to keep advancing.

`run` writes one line per transition on stdout, in order, and flushes it when the loop makes the transition; it does not wait for the command to end.
A started worker prints `<attempt id> started -> doing.running`; a started gate prints `<attempt id> verify started (pid <pid>)`, an approval prints `<task> approved -> review.integrating (<attempt id>)`, an integration prints `<task> integration started (pid <pid>)`, a conflict rework prints `<task> conflict with main -> todo.ready (automatic rework)`, or when the task has had `limits.max_conflict_rework` conflict reworks in a row, `<task> conflict with main -> review.rework_pending: <reason>`, an automatic reverify prints `<task> main moved (<short sha>) -> auto reverify`, or when the task used its count, `<task> main moved (<short sha>) -> auto reverify limit reached: <reason>`, a review whose model was unavailable prints `<task> review model unavailable -> retry at <UTC time>` when the stage fails and `<task> review retry_at passed -> auto reverify` when the pass claims its reverify (see [A review wait on an unavailable model](gate.md#a-review-wait-on-an-unavailable-model)), a worker whose model was unavailable prints `<task> worker model unavailable -> retry at <UTC time>` (see [A worker wait on an unavailable model](gate.md#a-worker-wait-on-an-unavailable-model)), and the plan's follow-up task arc proposes once the plan has no open task prints `<new task id> proposed (follow-ups from <plan id>)` (see [The review's follow-up](gate.md#the-reviews-follow-up)).
The limit's `<reason>` is the text `show` prints as the `rework_reason`: `the task used <n> automatic reverifies and `[limits] max_auto_reverify` is <max>`.
A task a plan cancel left at `doing.reconciling` prints `<task> doing.reconciling -> failed.cancelled (its plan is cancelled)`.
One line per `(head_sha, remote main)` pair is printed, so a summary that repeats every interval does not print it again.
When a merged task's own change touches arc's code, its merge line is followed by one more: `radian arc: <task> changed arc's code on main; rebuild radian (the loop restarts itself when its binary changes)`.
Arc's code is a path under `crates/radian-arc/`, a path under `crates/radian-tui/src/arc/`, or a path that starts with `crates/radian-tui/src/cli/arc`.
The paths are the task's diff against its merge base, which the merge already reads; no network call is added.
When a pass changes nothing, a summary of what waits on whom ends the pass:

```text
TK0001-a-1 started -> doing.running
summary
PL0001-plan (auto run)
  TK0001-a doing.running: running its attempt
  TK0002-b todo.ready: waiting for a free concurrency slot
```

A pass that wrote no transition line starts its summary with `no transition`.
A ready task whose flow is full says `waiting for a free <flow> slot` (for example `waiting for a free image slot`); a full [`limits.concurrency`](files.md#configtoml) says `waiting for a free concurrency slot`, and the flow's reason wins when both are full. A file without `[flows]` keeps the shared reason.
A file that changed outside arc is skipped and named in the summary; it does not stop the loop.
A plan whose budget or automatic-rework limit paused it is named with its reason.

`run` exits `0` when it ran to quiet, even when a task failed; `1` on a git or file error, `3` when the worker's `radian exec` line is rejected, and `4` when another process holds `runner.lock`. It never waits for the lock.

### `loop.log`

Every loop appends the lines it prints to `.radian/arc/loop.log`: the transition lines, the summary, and each `radian arc: <message>` error line, one line each with an RFC 3339 UTC timestamp and a space in front, flushed as it prints. Stdout and stderr are unchanged.
The file is not a second output: it is what the arc TUI's ticker and log view read, and it is there for a loop started by hand with `nohup` too.
At the first line after the file reaches 1 MiB it is renamed to `loop.log.1` (replacing an older one) and a new `loop.log` starts, so one old file is kept.
A write error on `loop.log` prints one `radian arc: cannot write …` line for the whole process, and the loop goes on.

## `radian arc serve`

`radian arc serve` is the scheduling loop: it takes `runner.lock`, repeats the pass `run` makes, and holds the lock for its whole life.
After every pass (which always ends quiet) it waits `--interval` seconds, then runs another pass, until SIGINT or SIGTERM; a second `run` or `serve` exits `4` as usual, and a TUI opened meanwhile starts nothing and names this loop's pid.

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--interval` | `SECONDS` | `30` | Whole seconds between passes, 5 to 3600. A value outside that range is a usage error (`2`). |

It is a loop, not a daemon: nothing supervises it, and while it is not running nothing advances until a person (or the TUI) starts one. Quitting a TUI that started it leaves it running.

A running `serve` follows a rebuilt binary by itself.
When it starts it records the path of its own executable (once, from `current_exe`; it never calls `current_exe` again) and that file's identity: device, inode, size and modification time.
After every pass, before it waits for the interval, it looks at the file at that path.
When the file exists, is a regular file with an execute bit, and its identity differs from the recorded one, it looks once more after one second and acts only when the second look is the same.
It then prints one line, `radian arc: the radian binary changed on disk; restarting the loop with it`, releases `runner.lock`, flushes [`loop.log`](#looplog), and replaces itself with `exec` of that path with the same arguments, working directory and environment.
The pid does not change, so the pid in `runner.lock` and `pgrep` stay meaningful.
It starts no gate, stops no gate and touches no worker: a worker and a gate are separate processes, and the new loop reconciles them exactly as it does after a killed loop.
A file that is missing or has no execute bit (a build in the middle of writing it) is ignored, and a later pass looks again; a file that changes between the two looks is not run either.
When `exec` fails, it prints one `radian arc:` line with the error, takes `runner.lock` again (when that fails it prints the line and exits `1`), records the new identity so it does not retry every pass, and goes on with the old code.
A one-shot `run` does not do this.
`radian arc mcp` does not do this either: after a rebuild, reconnect the lead's MCP server (see [`arc/mcp`](mcp.md)); it does not replace itself.

The pass also prints the `changed arc's code on main` line when its merge touches arc's code, with the three path prefixes and the meaning [under `run`](#radian-arc-run).

The summary is printed only when it differs from the summary the loop printed last, so a quiet loop does not repeat the same lines every interval. Every transition line is still printed when it happens.

A pass that returns an error prints one `radian arc: <message>` line and the loop waits its interval and runs the next pass; the lock and every state file stay as they are.
A one-shot `run` prints the same line and exits `1` instead.

A repeated error stops the loop: when three passes in a row return the same error text, it prints one line, `radian arc: the same error 3 passes in a row; stopping (fix it and start the loop again)`, stops the gates the way a stop signal does (see below; workers are left alone), releases `runner.lock`, and exits `1`.
A pass that succeeds, or a different error, starts the count again.
A one-shot `run` is unchanged.
The number `3` is a constant, not a configuration key.

On SIGINT or SIGTERM the loop starts no new worker and no new gate.
A Ctrl-C in the terminal reaches the loop only: each worker and each gate runs in its own process group, and both treat SIGINT exactly as SIGTERM.
A running worker is left alone: it is a separate process with its own limits and record, and the next pass (or the next `run`) collects it.
A running gate is signalled and gets `[limits] cancel_grace_secs` before SIGKILL, and so is one whose record is still `started` while it holds the gate lock; the gate forwards the signal to the stage it is running (the `commands` shell or the review session), waits for it, and exits without a record; once it is gone its record becomes `interrupted`, so a gate that did not finish is started again by the next loop as unverified and counts no death, and the verdicts it already wrote are reused, unless it exited before writing any record at all: then the next pass reports it and starts no gate while its inputs are unchanged (see [the gate run](gate.md#the-gate-run)).
The loop then prints the summary, releases `runner.lock`, and exits `0`.

It exits `0` when it ran to quiet, even when a task failed, and `0` on SIGINT or SIGTERM; `1` on a git or file error, `3` when the worker's `radian exec` line is rejected, and `4` when another process holds `runner.lock`. It never waits for the lock.

---
description: The arc scheduling loop: one pass, its output and loop.log, the hub, and its signals.
---

# arc: the loop

This page covers `radian arc run` and the scheduling loop the hub runs; the other `radian arc` commands are in [`arc`](index.md), the hub is in [`arc/hub`](hub.md), the gate is in [`arc/gate`](gate.md), [`arc/gate-review`](gate-review.md) and [`arc/gate-failures`](gate-failures.md), the TUI is in [`arc/tui`](tui.md), and the files arc keeps are in [`arc/files`](files.md).

## `radian arc run`

`run` is one pass of the scheduling loop without an interface; [`radian hub`](hub.md) repeats it for every registered repository.
It takes `runner.lock`, makes every transition the auto levels allow now, saves the state, and exits.
A pass only schedules: it starts a worker or a gate, collects what one left, and moves the state, so a pass ends in seconds.
Waiting for you, a `retry_at` in the future, a running worker, or a running gate does not keep it running.
A task of a plan at `auto` `off` is never started or merged by `run`; its verification still advances.
A task of a `cancelled` plan is never claimed, gated or merged, and a cancelled plan is not listed in the summary.

The arc TUI never runs this pass itself: when no hub holds this repository's lock it starts `radian hub` (see [`arc/tui`](tui.md#the-loop)). A TUI and a loop (`run` or the hub's) exclude each other through `runner.lock`: a second `run` that cannot get it exits `4`, and the hub records the repository as read-only.
Quitting the TUI never stops a loop, whether the TUI started it or not.

Before its first pass, and before each pass claims a task, it checks the worker's `radian acp` line the way `start` does: a rejected line exits `3`, claims nothing, and changes no file (see [`worker.exec_args`](config.md#configtoml)).

`run` takes no option. The old form `radian arc run --watch`, with or without `--interval`, is refused: the scheduling loop is now `radian hub`. It exits `2` with one stderr line, ``radian arc: `run --watch` is now `radian hub` ``.

One pass repeats until a pass changes nothing, in this order:

0. run every pending `[hooks]` point (see [Hooks](hooks.md#hooks)): the record is cleared after its commands ran, so a process killed mid-hook runs it again here, and a failing command prints one `warning:` line without stopping the pass;
1. reconcile every task in `doing` (see [Reconcile](gate.md#reconcile)): apply an `AttemptFinished` that is there, leave a live attempt alone, apply the retry rules to a dead one. A `doing.reconciling` task of a `cancelled` plan is ended at `failed.cancelled` after the same process check `cancel` makes;
2. move every `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`; this is also how a worker model wait ends (see [A worker wait on an unavailable model](gate-failures.md#a-worker-wait-on-an-unavailable-model));
3. re-verify, by itself, a task a `commands` or integration command failure blocked, once the target branch moved (see [A command failure and a moved target branch](gate-failures.md#a-command-failure-and-a-moved-target-branch)), and a task waiting on an unavailable review model once its `retry_at` has passed (see [A review wait on an unavailable model](gate-review.md#a-review-wait-on-an-unavailable-model)): claim a new attempt without a worker. The two guards of the moved-target path apply to it, and the review wait uses no count; neither is a retry: they count toward neither `max_attempts` nor `auto_rework`;
4. for every task in `review.verifying` (every plan, including a task that a moved branch or a changed digest sent back, and one step 3 just claimed): apply the verdicts a background gate left, or start its gate in the background;
5. for each confirmed plan at `auto` `run` or `merge`, claim every `todo.ready` task whose `after` tasks are merged, within its flow's [`flows.<name>.concurrency`](config.md#configtoml) and up to [`limits.concurrency`](config.md#configtoml) attempts in flight, and start each wrapper without waiting for it. A ready task whose flow is full is skipped, so a task of another flow further down the claim order is claimed in the same pass;
6. for each plan at `auto` `merge`, approve a verified task and start its integration in the background, or finish an integration a gate left.
   `run` writes the same `human` pass verdict `radian arc merge` writes, with `approver` `auto` in it.

Steps 4, 5 and 6 walk the backlog in *claim order*: the plan's `priority` (`P0` first), then the plan id from small to large, then the task's order in the plan and its `after` list.
So the higher priority's task is verified, claimed, merged, and queued for a stage's lock before a lower priority's.
`priority` only decides which plan gets a free concurrency slot: a running attempt is never preempted, so a changed priority takes effect at the next free slot.

The long work runs outside the pass: a worker is a separate `radian arc work` process with its own limits and record, and a gate is a separate process with its own record.
A stage this build does not run, or a branch that moved, is reported in the summary; the gate's refusal is recorded, and `run` starts no new gate for that attempt until the inputs change (for verification the branch comes back; for integration a new `head_sha`, a new target branch or a changed `config.toml`; or `start`/`reverify` claims a new attempt).
A gate whose process exited without writing any record is reported as `<task> verify cannot run: its gate exited without a record` (or `<task> integration cannot run: …`), recorded for that attempt; no new gate starts while its inputs stay the same, and `show` prints the same text as the `rework_reason` (see [the gate run](gate.md#the-gate-run)).
A start is counted exactly once: a gate whose record is `running`, or `started` with no inputs, and whose gate lock is free is one death, and it is started again; after [`limits.max_gate_deaths`](config.md#configtoml) deaths for the same attempt and inputs the record becomes `died`, the summary prints `<task> verify cannot run: its gate died after it started`, `show` prints the same text as the `rework_reason`, and no gate starts while the inputs stay the same. A gate the loop stopped itself is `interrupted` instead, and the next loop starts it again without counting. A gate whose record is `started` with the inputs it was planned with is the one start that is not a death: it is reported as `no_record` above.
The next pass applies a wrapper's early failure (a refused argument, a held worktree lock) the same way, so its concurrency slot is free at once (see [Reconcile](gate.md#reconcile)).

A one-shot `run` starts its work and exits without waiting for it; the next `run` collects the result.
Use [`radian hub`](hub.md) to keep advancing.

`run` writes one line per transition on stdout, in order, and flushes it when the loop makes the transition; it does not wait for the command to end.
A started worker prints `<attempt id> started -> doing.running`; a started gate prints `<attempt id> verify started (pid <pid>)`, an approval prints `<task> approved -> review.integrating (<attempt id>)`, an integration prints `<task> integration started (pid <pid>)`, a conflict rework prints `<task> conflict with main -> todo.ready (automatic rework)`, or when the task has had `limits.max_conflict_rework` conflict reworks in a row, `<task> conflict with main -> review.rework_pending: <reason>`, an automatic reverify prints `<task> <target branch> moved (<short sha>) -> auto reverify`, or when the task used its count, `<task> <target branch> moved (<short sha>) -> auto reverify limit reached: <reason>`, a review whose model was unavailable prints `<task> review model unavailable -> retry at <UTC time>` when the stage fails and `<task> review retry_at passed -> auto reverify` when the pass claims its reverify (see [A review wait on an unavailable model](gate-review.md#a-review-wait-on-an-unavailable-model)), a worker whose model was unavailable prints `<task> worker model unavailable -> retry at <UTC time>` (see [A worker wait on an unavailable model](gate-failures.md#a-worker-wait-on-an-unavailable-model)), and the plan's follow-up task arc proposes once the plan has no open task prints `<new task id> proposed (follow-ups from <plan id>)` (see [The review's follow-up](gate-review.md#the-reviews-follow-up)).
The limit's `<reason>` is the text `show` prints as the `rework_reason`: `the task used <n> automatic reverifies and `[limits] max_auto_reverify` is <max>`.
The `<target branch>` of an automatic reverify line is `[integration] target_branch`, or `[repository] main_branch` when it is not set ([config.toml](config.md#configtoml)): with `target_branch = "release"`, the line names `release` rather than an untouched `main`.
A task a plan cancel left at `doing.reconciling` prints `<task> doing.reconciling -> failed.cancelled (its plan is cancelled)`.
One line per `(head_sha, remote target branch)` pair is printed, so a summary that repeats every interval does not print it again.
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
A ready task whose flow is full says `waiting for a free <flow> slot` (for example `waiting for a free image slot`); a full [`limits.concurrency`](config.md#configtoml) says `waiting for a free concurrency slot`, and the flow's reason wins when both are full. A file without `[flows]` keeps the shared reason.
A file that changed outside arc is skipped and named in the summary; it does not stop the loop.
A plan whose budget or automatic-rework limit paused it is named with its reason.

A task the pass finds startable but does not start begins a queue wait.
It writes `runs/<task>/eligible_at.json` the first time a pass finds it startable and leaves it unstarted: a full [`limits.concurrency`](config.md#configtoml), a full [`flows.<name>.concurrency`](config.md#configtoml), the hub's machine-wide [`hub.concurrency`](config.md#configtoml) cap, or a claim another writer refused.
The file is written once and never rewritten, so the wait keeps the first pass's time.
The claim begins the attempt's `runs/<task>/attempts/<attempt id>/queue.json` and deletes the file; the worker wrapper completes that record with `started_at` (the value `execution.json` carries) and `queue_wait_secs` (whole seconds) when it starts the worker, or with the time it ended the attempt when it ends it before a worker starts. A `radian arc start` of a task with no such file waits `0`. A task that leaves `todo.ready` another way, and every task of a plan that stops starting work (a pause, `auto` `off`, an amendment back to `draft`, a budget or automatic-rework pause), loses the file and starts a fresh wait when it returns (see [an attempt's queue wait](files.md#an-attempts-queue-wait)).
The wait resolves to the pass interval: `hub.interval_secs` for the hub's loop, and one pass of a one-shot `radian arc run`.
`radian arc show` prints it beside the worker's `wall`.

`run` exits `0` when it ran to quiet, even when a task failed; `1` on a git or file error, `3` when the worker's `radian acp` line is rejected, and `4` when another process holds `runner.lock`. It never waits for the lock.

### `loop.log`

Every loop appends the lines it prints to `.radian/arc/loop.log`: the transition lines, the summary, and each `radian arc: <message>` error line, one line each with an RFC 3339 UTC timestamp and a space in front, flushed as it prints. Stdout and stderr are unchanged.
The file is not a second output: it is what the arc TUI's ticker and log view read, and it is there for a loop started by hand with `nohup` too.
At the first line after the file reaches 1 MiB it is renamed to `loop.log.1` (replacing an older one) and a new `loop.log` starts, so one old file is kept.
A write error on `loop.log` prints one `radian arc: cannot write …` line for the whole process, and the loop goes on.

## The hub's loop

`radian arc serve` was removed: the scheduling loop is `radian hub`, which runs one loop per registered repository.
The `hub.lock`, the registry re-read, the OS threads, the machine-wide [`hub.concurrency`](config.md#configtoml), the status lines, the signal handling and the rebuilt-binary restart are in [`arc/hub`](hub.md#the-loop).
A repository's loop follows the same rules `run` obeys here: it holds `runner.lock` for its life, prints the summary only when it differs from the last one, writes every line to [`loop.log`](#looplog), and a pass that returns an error prints one `radian arc: <message>` line and continues with the next pass.
After three identical errors in a row that repository's loop stops (it stops the gates it started the way a signal does, releases `runner.lock`, and the hub keeps every other loop running); restart the hub to schedule that repository again.
A one-shot `run` prints the same error line and exits `1` instead.

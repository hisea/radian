---
description: The arc gate: the stage overview, a gate for this repository, and reconcile.
---

# arc: the gate

This page covers the gate run and its stages; the review stage is in [`arc/gate-review`](gate-review.md), the failures, retries and the budget are in [`arc/gate-failures`](gate-failures.md), the commands are in [`arc`](index.md), the scheduling loop is in [`arc/loop`](loop.md), and the files arc keeps are in [`arc/files`](files.md).

## The gate run

A task in `review.verifying` is checked against `verified_sha`, which is the pushed `head_sha` of the task's current attempt.
`start` runs the gate in the same process after a pushed attempt, and `radian arc run` starts it as a background process: a pass starts the gate and returns, and a later pass reads its verdicts and moves the task ([`run`](loop.md#radian-arc-run)).
A gate writes its verdict files and one record, `runs/<task>/gate.json`; it never changes the task's state itself.

- arc fetches `[repository] remote` first.
  The remote's `work/<task>` must still be `head_sha`.
  When it differs, arc records the reason in the attempt's `review.json` and the task stays in `review.verifying`; `show` prints the reason.
  The fetch is a git call that talks to a remote, so [`limits.git_remote_timeout_secs`](config.md#configtoml) bounds it; a fetch that runs out fails the gate with `git fetch timed out after <n> s`.
  arc's own fetches into a repository take turns on a lock in its git common dir (see [`radian arc init`](planning.md#radian-arc-init-track)); a fetch that waits for the lock past the same limit fails with a line that names the lock. A `git fetch` you run yourself is not serialized by it.
- The stages are the task's profile's (`gate.profiles.<name>.stages`), in order, plus the task's `extra_stages`.
  A task's profile is its flow's [`flows.<name>.gate_profile`](config.md#configtoml); a repository without `[flows]` uses `gate.profile`.
  `human` is your approval and does not run here.
  When the profile lists both `commands` and `review`, those two start at the same time; every other stage keeps its order.
- Every stage runs against the verification candidate, in its own checkout under `.radian/arc/gates/<stage>/`.
  The review stage's checkouts are one per concurrency slot, `.radian/arc/gates/review-<n>/`.
  The candidate is `verified_sha` when the remote target branch is an ancestor of it; otherwise it is a merge commit of `verified_sha` onto the remote target branch, built in that checkout and never pushed.
  The merge uses arc's own commit identity, so the same inputs build the same tree.
  `verified_sha` stays the task's commit: the approval, the digest and `merge` bind to it, and the candidate is evidence only.
  A stage's commands run in order and stop at the first failure; each has its own log and the profile's timeout.
- A verification stage's verdict records `sha` (the task's `verified_sha`), `main_sha` (the remote target branch the candidate was built on), `candidate_sha` (the commit the stage ran on; absent when the merge conflicted) and `profile` (the profile the stage ran under).
- A merge conflict is a failed stage: the verdict's reason is `conflict`, its `conflict_paths` lists the paths, the task goes to `review.rework_pending`, and `show` prints `conflict: <paths>`.
  Nothing is pushed.
- This build runs `commands` and `review`.
  A profile or a task that lists `mutation` or `ci` is refused with `stage <name> is not built yet`; the task stays in `review.verifying` and `show` prints that reason.
  `init`'s default profile lists only `commands` and `human`.
- A verdict is `runs/<task>/attempts/<attempt id>/verdicts/<stage>.json`; a command's log is `runs/<task>/attempts/<attempt id>/logs/<stage>-<n>.log`.
  A verdict is written once and never changed.
  A verdict that already exists for this attempt and stage is reused instead of rerun, so a crash between stages resumes where it stopped; when the `commands`/`review` pair was interrupted, only the stage without a verdict runs again.
- One gate runs per task at a time.
  The gate process holds `gates/<task>.gate.lock` from the moment a pass starts it, so the pass that follows starts no second gate for the same commit.
  A signalled or killed gate releases the lock; the next pass starts it again, and the verdicts already written are reused.
  Stages of different tasks still queue on their own locks: one `commands` stage at a time, one checkout per stage, and the review stage has its own.
  Up to [`gate.review.concurrency`](config.md#configtoml) review sessions run at once (default `2`, `1` or more); each takes one of the locks `gates/review-0.lock` … `gates/review-<n-1>.lock` and the checkout of that slot, so no two sessions share a checkout. With `1`, a second review waits for the first.
  The `review` stage is a read-only session, so it does not wait for another task's `commands`.
- The gate's own output is `runs/<task>/gate.log`.
- `runs/<task>/gate.json` holds one record, with the attempt it ran for.
  Its `status` is one of `started`, `running`, `done`, `refused`, `error`, `no_record`, `died` and `interrupted`.
  A pass applies a `done` record; reports a `refused` one in the summary and starts no new gate for that attempt until the inputs change (for verification the branch comes back; for integration a new `head_sha`, a new target branch or a changed `config.toml`; or `start`/`reverify` claims a new attempt); reports an `error` one as an error (exit `1` for a one-shot `run`); and prints the text of a `no_record` or `died` one as the reason the verification cannot run.
  `radian arc show` keeps printing the recorded reason.
- Before a pass starts a gate it writes to that record the inputs the gate will read, when it could plan them, and how many times a gate already died for them (`status` `started`), and names the gate process there once it exists; the gate replaces the status with `running`, keeps the inputs and the count, and an integration gate is the same.
  A gate that exits before writing any record leaves `started`: when that record carries the inputs, the next pass records `no_record` for that attempt with them, reports it in the summary as `<task> verify cannot run: its gate exited without a record`, starts no gate while they are unchanged, and does not count the pass as changed. A new `head_sha`, a new target branch or a changed `config.toml` allows one new start. A pass that could not plan the gate writes no inputs; that start is a death (next line), not a `no_record`.
- A start is counted exactly once: the record behind it is `running`, or `started` with no inputs, and the gate lock is free (while the lock is held the gate may still be starting or running, and the pass starts nothing). Either way the pass has seen one death: the record's `deaths` goes up by one for that attempt, and the gate is started again.
  When the count would pass [`limits.max_gate_deaths`](config.md#configtoml) (integer, default `2`, `0` or more), the pass starts no gate and records `died` instead, which behaves exactly like `no_record`: the summary prints `<task> verify cannot run: its gate died after it started`, `show` prints the same text as the `rework_reason`, no gate starts while the inputs are unchanged, and the pass does not count as changed.
  A new `head_sha`, a new target branch or a changed `config.toml` allows one new start and begins again at 0 deaths; `0` stops at the first death, so the gate is started `1 + max_gate_deaths` times at most, whether or not the pass could plan it.
  The inputs decide only whether the count carries: a digest on both sides that differs begins the count again at `1`, and when either side has none (the gate could not be planned then, or cannot be planned now) there is nothing to compare, so the count carries forward. A `died` record written when the pass has no inputs keeps none, so a later pass that can plan once more starts one new gate.
- A gate that the scheduling loop stops itself is `interrupted`, not a death.
  On SIGINT or SIGTERM the loop stops the gate it finds (also one whose record is still `started` while it holds the gate lock), waits `[limits] cancel_grace_secs` and kills it; once the gate process is gone the record becomes `interrupted`, keeping the attempt and the inputs.
  The next pass starts the gate again at once, reuses the verdicts already written, and counts nothing.
- Any stage failing moves the task to `review.rework_pending`.
  The stage, its reason and the verdict path are in the task's `rework_reason` (`show` prints it).
  Main moving after a verification does not send the task back to `review.verifying` by itself; `merge` integrates onto the then-current target branch and runs the key commands there.
- Every required stage passing moves the task to `review.awaiting_human` and records `verified_sha` and `verdict_set_digest` in the task.
  The digest covers the verification verdicts for `verified_sha`; it leaves out the `human` approval and the `integration` verdict, which are written later, at merge.
  A new verdict for the same commit changes the digest and invalidates an approval.

### An empty change

A task's own change is empty when the diff from the merge base of the remote target branch and `verified_sha` to `verified_sha` names no path; a commit that adds no content on top of the target branch is empty too.
A task whose brief allows it, such as the review's follow-up, may deliver nothing, and no stage fails it for being empty:

- `commands` runs the profile's commands on the candidate; when the target branch has not moved the candidate is the remote target branch, and nothing about the empty change fails it.
- `mutation` has nothing to mutate, so it passes; this build does not run it, and a profile that lists it is refused as unbuilt (see above), which is not a failure of the empty change.
- `review` passes without starting a session. Its verdict is a pass whose `review` report has a summary saying that the task's change is empty and empty `blocking`, `suggestions` and `verified`, with no session ids and no cost.
- `human` is your approval and does not change: it still binds to `verified_sha` and `verdict_set_digest`.
- `integration` builds the candidate with the `[integration] style` (a merge commit, or the target branch when it did not move, for `merge`; one commit for `squash`; the replayed commits for `rebase`) and runs the current `[integration] key_commands` on it as usual.

A task's `allow_noop` is its own value when its file carries one, else its flow's [`flows.<name>.allow_noop`](config.md#configtoml); the delivery check of each attempt reads it, at the attempt's claim (see [Task fields](files.md#task-fields)).

`list` and `show` do not use the network: they do not check the remote and cannot see a branch that moved after the gate.
`merge` checks the remote before it acts.

## A gate for this repository

Two bash scripts in this repository are the gate. A gate profile's `commands` call them:

```toml
[gate.profiles.default]
commands = [
  "/abs/path/to/repo/scripts/arc-gate.sh quick",
  "/abs/path/to/repo/scripts/arc-gate.sh full",
]
```

| Script | What it runs |
|---|---|
| `scripts/verify.sh [--log-dir <dir>]` | The full Verify of this repository: `cargo build -p radian-tui --bin radian --locked`, `cargo build -p agent-extensions --bin mcp-fixture --locked`, every command of the `Verify` block in `AGENTS.md`, then the three provider contract suites. It runs every step even after a failure, prints one `rc=<n> <seconds>s <command>` line per step, and keeps each step's output in `<dir>` (default `.radian/verify/`). It exits `0` only when every step passed, else `1`. |
| `scripts/arc-gate.sh quick` | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, the two binary builds, then `cargo nextest run --locked --no-fail-fast` for every crate under `crates/` that the checkout changed since its merge base with `origin/main`, plus `radian-tui` always. It stops at the first failing step. |
| `scripts/arc-gate.sh full` | `scripts/verify.sh`, here or on `ARC_GATE_HOST` when that is set. It prints the step lines and the names of failing tests, and exits with the verify exit code. |

Before their first step, `verify.sh` and `arc-gate.sh` check the file system that holds `${TMPDIR:-/tmp}` with `df`: `quick` always, `full` when it runs here. When fewer than 50,000 inodes or fewer than 2 GiB are free, they fail at once, before building anything, with exit code `1` and one line on stderr that names the path, the free and total inodes, the free space and the two limits, for example:

```
verify: /tmp has 0 of 1048576 inodes free (limit 50000) and 45 GiB free (limit 2 GiB); remove leaked test directories first
```

The check deletes nothing. A file system that reports no usable inode count (some macOS volumes report none, `-`, or a number too large for the shell to compare) skips the inode check and keeps the space check. A `df` that cannot read the file system skips that check with a warning. A negative free space (GNU `df` on ext4 when fewer bytes are free than the reserved-block count) is a real value and fails the space check; it is not read as unreadable. The two limits are the two variables below. For arc the failure is an ordinary failed command (`command_failed`), not a new failure class.

With `ARC_GATE_HOST` set, `full` ships `HEAD` as a git bundle (`HEAD --not origin/main`; no bundle when `HEAD` is already in `origin/main`). On that host it takes a blocking lock on `~/.arc-gate.lock`, fetches the bundle, checks out exactly that commit detached inside `ARC_GATE_TREE`, and runs that checkout's `scripts/verify.sh`. It does not check the local temp file system: the remote `verify.sh` checks the host's own `${TMPDIR:-/tmp}`, and the remote environment supplies both limits (they are not forwarded over ssh).

| Environment variable | Type | Default | What it does |
|---|---|---|---|
| `ARC_GATE_HOST` | string | unset: `full` runs in this checkout | The ssh host that runs the full gate. |
| `ARC_GATE_TREE` | string | unset | Required when `ARC_GATE_HOST` is set: the gate checkout on that host, as a path relative to the remote home directory. |
| `RADIAN_VERIFY_MIN_FREE_INODES` | integer | `50000` | A whole number, `0` or more and at most `999999999999999999` (18 digits after leading zeros); `0` turns the inode check off. The fewest free inodes on the temp file system at which `verify.sh` and `arc-gate.sh` start their first step. Unset or empty uses the default. It takes effect on the next run of either script, in the environment that script runs in (with `ARC_GATE_HOST` that is the remote environment). Any other value (not all digits, or above that bound) fails the check with a message naming the variable. |
| `RADIAN_VERIFY_MIN_FREE_GIB` | integer | `2` | A whole number, `0` or more and at most `999999999999999999` (18 digits after leading zeros); `0` turns the space check off. The fewest free GiB on the temp file system at which `verify.sh` and `arc-gate.sh` start their first step. Unset or empty uses the default. It takes effect on the next run of either script, in the environment that script runs in (with `ARC_GATE_HOST` that is the remote environment). Any other value (not all digits, or above that bound) fails the check with a message naming the variable. |

Neither script sets `CARGO_TARGET_DIR`: a build cache is the caller's setting.

**Trust.** arc runs a gate's `commands` inside a checkout of the task's own commit.
A relative `scripts/...` path there would name the task's own script, so a task could change its own gate.
Call both scripts by the main checkout's absolute path (`/abs/path/to/repo/scripts/arc-gate.sh`), never a relative path.

## Reconcile

arc keeps no attempt state in memory.
Before `start` or `cancel` acts, and after a restart, it reconciles every task in `doing`:

- a wrapper that failed after its attempt directory existed wrote its own end record before it exited, whatever the failure: a refused argument, a worktree lock another attempt holds, a fetch that failed, a worktree that could not be prepared, or a worker that could not start. The record is applied at once: the attempt's concurrency slot is free in the next pass, the class is an infrastructure failure, and the [retry rules](gate-failures.md#failures-retries-and-the-budget) apply. A retry is not counted as a rework;
- only a failure before those directories exist (a task id that names no path, a directory that cannot be created) leaves no record. That attempt is applied as an infrastructure failure when its heartbeat is older than the stall window, as below;
- the wrapper's `attempt_finished.json` is present and its token is current: the result is applied;
- the wrapper's last heartbeat (`attempts/<task>/<attempt>/heartbeat.json`) is younger than the attempt's stall window (its `stall_timeout_secs`, else `[limits] stall_timeout_secs`): the attempt is alive and left alone.
  The wrapper beats at most every 15 seconds, and at a third of the stall window when that is shorter, so three beats always fit inside the attempt's stall window; the gap between two beats is that interval.
  It beats from the moment the attempt directory exists until both end records are written; it keeps beating while the wrapper sets up the worktree, waits for the model, or checks and pushes the result.
  When both stall values are `0` the window is three beats, 45 seconds.
  A claim whose wrapper has not beaten yet is judged by its `start.json`;
- the heartbeat is older than that window and there is no result: the attempt is an `infrastructure` (terminated) failure and the [retry rules](gate-failures.md#failures-retries-and-the-budget) apply.

Reconcile never starts a task again.
It reads no process list; `ps` is refused inside the `workspace` sandbox and is never needed here.
`doing.reconciling` is left for you to resolve; `radian arc cancel` moves it to `failed.cancelled` after the same process check.
The one exception is a task of a `cancelled` plan: the plan is final, so the next `radian arc run` pass ends it at `failed.cancelled` after that same process check, and it stops counting against `[limits] concurrency`.


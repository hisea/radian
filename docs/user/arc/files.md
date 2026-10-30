---
description: The files arc keeps: plan and task files, the worker brief, and the .radian/arc/ layout.
---

# arc: the files

This page covers the plan and task files, the worker brief and the layout arc keeps; `config.toml` is in [`arc/config`](config.md), the commands are in [`arc`](index.md), the scheduling loop is in [`arc/loop`](loop.md), the gate is in [`arc/gate`](gate.md) and its failures in [`arc/gate-failures`](gate-failures.md).

## `worker-brief.md`

`<repository>/.radian/arc/worker-brief.md` is the repository's standing worker instructions.
It is optional.
[`radian arc init`](planning.md#radian-arc-init-track) writes it with this default when the file does not exist, and never overwrites a file you edited:

```text
Work only on the current branch.
Commit everything that belongs to the change, and end with `git status --short` printing nothing.
Put scratch files only under `.radian/`.
Never change `.radian/arc/`.
While iterating, run only the tests being changed, and run each named crate in full once at the end; never the whole workspace.
In a `MUTATIONS.md`, add a new section at the end of the file, with one blank line before its `##` heading.
End with a short report: the commits, the tests you ran, the decisions you made, and any open questions.
```

When the file exists and is not empty after trimming, every brief a worker receives ends with its content: after the `merge-main` conflict section, the hand-off section of a fresh rework, and the task's own brief.
That covers a first attempt, a rework (fresh or resume), and a follow-up task arc wrote.
The wrapper reads the file when it composes an attempt's brief, so an edit takes effect at the next attempt; no restart is needed.
A missing file and one that is empty after trimming change nothing.

A file over 32 KiB (32,768 bytes), a file that is not UTF-8, a path that is not a regular file (a FIFO, a directory, a device), and a read error are not appended: the attempt runs without the section, its record carries one line `worker-brief.md skipped: <reason>`, and [`radian arc show`](planning.md#radian-arc-show-id-json) prints it as `attempt_note`.
The worker never reads the file itself: its sandbox denies `.radian/arc/`, and the wrapper puts the content on the worker's stdin.

A brief that points at a file under `.radian/arc/runs/` gets a copy as well: when the wrapper composes the brief, it copies the file into the worker's worktree at `.radian/arc-verdicts/<task>/<attempt>/<file>` and puts that worktree-relative path in the brief, so the worker reads the copy and the sandbox rule does not change.
The task file keeps the absolute path.
A file that is missing, is not a regular file, is larger than 1 MiB, or cannot be copied stays in the brief as the path it already had, and the attempt record carries `verdict copy skipped: <path>: <reason>` in its notes, next to the `worker-brief.md skipped:` line.

## Plan and task files

A plan file is `plans/<id>.md`; a task file is `tasks/<id>.md`.
Both are front matter between two `---` lines, then a body.
arc is the only writer; you change them with [the `radian arc` subcommands](index.md), and `adopt` after a hand edit.

An id is a prefix of 1 to 4 uppercase ASCII letters, a four-digit number, a dash, and a slug (`PL0001-first`, `TK0001-work`).
`[ids] plan_prefix` (default `PL`) and `[ids] task_prefix` (default `TK`) set the two prefixes; they must differ, and a prefix that is empty, not 1 to 4 uppercase letters, or equal to the other is refused by name when `config.toml` is read.
Plans and tasks count in separate number sequences: a new id takes the next free number above the highest number any item of that kind (a file or a ledger entry) uses, whatever prefix that item carries, and the number is never reused while its ledger entry exists.
An id without a prefix is not an id (exit `2`).
Any prefix of 1 to 4 uppercase letters is a well-formed id, and the kind comes from the directory it is looked up in (`plans/` or `tasks/`), not from the prefix: an item created under an earlier prefix keeps working after the prefix changes.
The id is the file name and never changes.
An id names one item: `show`, `adopt`, `reset-budget`, `cancel`, and the MCP `show` tool refuse an id that names both a plan file and a task file with exit `3` (the MCP tool: a tool error), name both files, and change nothing.

### Plan fields

| Field | Type | Required | Allowed values |
|---|---|---|---|
| `schema_version` | integer | yes | `1` |
| `id` | string | yes | `<PREFIX><NNNN>-<slug>`, prefix `PL` by default |
| `title` | string | yes | any text |
| `status` | string | yes | `draft`, `confirmed`, `cancelled` |
| `auto` | string | yes | `off`, `run`, `merge`; only a `confirmed` plan may use `run` or `merge` |
| `priority` | string | no | `P0`, `P1`, `P2`, `P3`, `P4`, `P5`; `P0` is the highest; a file without the field reads as `P3` |
| `max_cost_usd` | number | no | 0 or more, US dollars |
| `max_wall_time_secs` | integer | no | seconds |
| `tasks` | list of ids | yes | the plan's tasks, in display order (`[]` when there are none) |

The body is the request text, then a `## Change records` section.
Each change record is a `### <UTC time>` line (the shape `YYYY-MM-DDTHH:MM:SSZ`), a blank line, and the text of the change.
A confirmed plan gains records only; a draft plan may have its request text replaced, which keeps the records below it.
A cancelled plan takes no change record: it is final.
`priority` is written only once you set it with [`radian arc priority`](owner.md#radian-arc-priority-plan-priority); until then the field is absent and the plan reads as `P3`, so an older plan file is written back unchanged.

### Task fields

| Field | Type | Required | Allowed values |
|---|---|---|---|
| `schema_version` | integer | yes | `1` |
| `id` | string | yes | `<PREFIX><NNNN>-<slug>`, prefix `TK` by default |
| `plan` | string | yes | the plan's id |
| `title` | string | yes | any text |
| `state` | string | yes | `todo`, `doing`, `review`, `done`, `failed` |
| `phase` | string | yes | one of the phases of the task's state, in the table below |
| `after` | list of ids | yes | tasks that must be merged first (`[]` when there are none) |
| `flow` | string | no | the name of one `[flows.<name>]` section; absent reads `default`. It can change only while `attempt` is `0` (`update_task`, `adopt`); after the first attempt a change is refused and a new task is the way to run a different worker. |
| `supersedes` | string | no | the id of a task this one replaces |
| `attempt` | integer | yes | 0 or more; `0` when the task is new |
| `attempt_id` | string | no | the current attempt's `<task>-<n>`, written by the claim |
| `token` | string | no | the current attempt's random token; a result is applied only when it matches |
| `cost_usd` | number | no | the task's counted cost in US dollars: every worker attempt and every `review` stage, summed; the budget compares this number, and `reset-budget` makes it `0` |
| `total_cost_usd` | number | no | the task's cost in US dollars: every worker attempt and every `review` stage, summed; `reset-budget` does not change it. A file without the field reads its `cost_usd` as the total until its next write. |
| `cost_unknown` | boolean | no | `true`, `false`; `false` when absent. arc writes `true` when an attempt ends without reporting a `cost` (a [`worker.command`](config.md#configtoml) that writes none, or a flow's [`acp`](config.md#configtoml) agent, whose protocol has no fee), so the task's and its plans' cost sums are incomplete: `show`, `list` and the arc TUI print `unknown` for them. A later attempt that reports a cost does not clear it, and `reset-budget` does not either; an attempt that is still running has no cost and leaves the field as it is. An outside change to it is refused by `adopt`, like the other fields only arc sets. |
| `auto_rework` | integer | no | `0` or more; how many reworks arc counted while the plan's `auto` was not `off`; `0` when absent |
| `retry_count` | integer | no | `0` or more; how many retries arc started since the task's last lead or review rework; `0` when absent. The lead's brief and the review rework set it back to `0`; a conflict rework keeps it, and `max_attempts` counts it. |
| `allow_noop` | boolean | no | `true`, `false`; absent uses [`flows.<name>.allow_noop`](config.md#configtoml), and `false` for the implicit `default` flow of a file without `[flows]`. An explicit `false` is the task's own and does not fall back. |
| `followup_pending` | boolean | no | `true`, `false`; `false` when absent. arc writes `true` when the task reaches `done.merged` on any merge path, while [`gate.review.propose_followups`](config.md#configtoml) is `true` and the task is not itself a follow-up (see [The review's follow-up](gate-review.md#the-reviews-follow-up)). Only a merged task that carries it is collected, and only once: a task merged before the field existed, or merged while the switch was `false`, has no field and is never collected. An outside change to it is refused by `adopt`, like the other fields only arc sets. |
| `pending_hooks` | list of strings | no | Hook point names (`after_merge`, `after_fail`, `after_rework`, `after_awaiting_human`, `after_proposed`); `[]` when absent. The point a transition owes; the same write that moves the task adds it, and it is removed only after the point's commands ran, success or failure. A file from before the field reads as `[]` and runs no hook. An outside change to it is refused by `adopt`, like the other fields only arc sets. |
| `pending_hook_states` | list of strings | no | One `<point>=<state>.<phase>` entry per pending point (`after_fail=failed.exhausted`); `[]` when absent. The phase the point fires for, recorded by the same write that adds the point, so `ARC_STATE` names the phase the transition entered even after the task moved again. A point without an entry reports its own phase, so a file from before the field still runs its hooks. An outside change to it is refused by `adopt`, like the other fields only arc sets. |
| `merged_sha` | string | no | The sha the last merge put on the target branch: the pushed candidate, or the sha an `[integration] command` reported; absent when the task did not merge. The `after_merge` hook reads it as `ARC_MERGED_SHA`. An outside change to it is refused by `adopt`, like the other fields only arc sets. |
| `extra_stages` | list of strings | yes | gate stages added to the profile (`[]` when there are none) |
| `max_attempts` | integer | no | 1 or more; overrides `limits.max_attempts` for this task |
| `max_cost_usd` | number | no | 0 or more, US dollars; overrides the plan limit |
| `max_wall_time_secs` | integer | no | seconds; overrides the plan limit |
| `stall_timeout_secs` | integer | no | seconds; overrides `limits.stall_timeout_secs` |
| `base_sha` | string | no | the commit the task branch started from; filled when the task is claimed |
| `head_sha` | string | no | the task branch's tip that the attempt pushed; filled when a pushed attempt ends |
| `verified_sha` | string | no | the commit the gate verified; filled by verification |
| `verdict_set_digest` | string | no | the SHA-256 of the verification verdict set that made the task `review.awaiting_human`; a merge recomputes it |
| `retry_at` | string | no | an absolute UTC time, `YYYY-MM-DDTHH:MM:SSZ` |

The body is the original brief, then zero or more `## Rework <n>` sections, then the `## Changes` section.
The MCP `update_task` tool replaces the original brief and keeps every rework section and the change records below it.
`## Changes` holds one line per successful `update_task` call, in the order they happened: `- <UTC time> update_task: <the fields the call gave, comma-separated>`, where the time has the shape `YYYY-MM-DDTHH:MM:SSZ`; the first call creates the section.

### States and phases

| State | Phases |
|---|---|
| `todo` | `ready`, `blocked`, `retry_wait`, `paused_budget`, `proposed` |
| `doing` | `starting`, `running`, `reconciling` |
| `review` | `verifying`, `rework_pending`, `awaiting_human`, `integrating`, `ready_to_merge` |
| `done` | `merged` |
| `failed` | `exhausted`, `cancelled`, `irrecoverable` |

A new task is `todo` / `ready`.
A `todo` / `retry_wait` task is claimed at once by `radian arc start`, without waiting for its `retry_at`; the scheduling loop waits for `retry_at` (see [`radian arc start`](running.md#radian-arc-start-task)).
A task the lead adds to a confirmed plan (MCP `add_task`) or arc proposes as the plan's review follow-up is `todo` / `proposed`: the scheduling loop never claims it and `start` refuses it until you run `radian arc confirm <task>`.
A plan is done when all of its tasks are merged; `done` is not a stored plan status.
A `cancelled` plan is final: `radian arc cancel <plan>` sets it after ending every task `cancel` accepts in the plan (see [`radian arc cancel`](running.md#radian-arc-cancel-id)).
No command confirms it, adds a task to it, changes its `auto` or `priority`, resets its budget, or starts, reverifies or gates its tasks, and `list` hides it unless `--all`.

### The review states

A task leaves `doing` for `review.verifying` when a pushed attempt ends.

| From | To | What moves it |
|---|---|---|
| `review.verifying` | `review.awaiting_human` | Every required non-`human` stage passed for `verified_sha`; `verified_sha` and `verdict_set_digest` are recorded. |
| `review.verifying` | `review.rework_pending` | A stage failed: the stage, its reason and the verdict are recorded. |
| `review.awaiting_human` | `review.integrating` | `radian arc merge`, or `radian arc run` on a plan at `auto` `merge`, with `verified_sha` still on the remote, the digest unchanged, and every required stage passed. |
| `review.awaiting_human` | `review.rework_pending` | `radian arc reject --note <text>`: the note is recorded. |
| `review.integrating` | `review.ready_to_merge` | The integration stage passed against the current target branch; its `base_sha` and `candidate_sha` are in the integration verdict. |
| `review.integrating` | `review.rework_pending` | A conflict, a failing key command, or a `rebase` whose branch contains a merge commit. |
| `review.ready_to_merge` | `done.merged` | The merge landed what the gate verified: for `push`, the target branch is still the integration `base_sha` and the push succeeded; for `command`, the command check passed ([the command check](integration.md#the-command-check)). The write records the `after_merge` hook (and `merged_sha`) in the task. |
| `review.ready_to_merge` | `review.rework_pending` | The merge does not hold what the gate verified: a failed after-push check, a command answer that fails the check, `closed`, a non-zero exit, a timeout, or any other last line. The reason is recorded. |
| `review.ready_to_merge` | `review.integrating` | The target branch moved after integration, the push was rejected as non-fast-forward, or a merge command answered `pending`: integrate again, or run the command again. |
| `review.awaiting_human` | `review.verifying` | The remote `work/<task>` is no longer `verified_sha`, or the verdict set no longer matches the recorded digest: every verdict and the approval of the old commit are invalid. |
| `review.rework_pending`, `review.awaiting_human` | `review.verifying` | `radian arc reverify`, or `radian arc run` reverifying a `commands` or integration command failure by itself once the target branch moved: a new attempt is claimed without a worker; its verdicts are its own. |
| `review.verifying` | `review.awaiting_human` (again) | `radian arc run` or `radian arc start` re-verifies a task a moved branch or a changed digest sent back. |
| `review` (any phase) | `todo.ready` | The lead adds a rework brief (`add_rework`); the next claim starts the next attempt. |
| `review` (any phase) | `failed.cancelled` | `radian arc cancel <task>`; a running gate is signalled first. |
| `todo.proposed` | `todo.ready` | `radian arc confirm <task>`: the task is confirmed, and its plan's `status` and `auto` do not change. |
| `todo.proposed` | `failed.cancelled` | `radian arc cancel <task>`. |

## The `.radian/arc/` layout

Everything arc stores for one repository is in `<repository>/.radian/arc/`:

| Path | What it holds |
|---|---|
| `config.toml` | The settings in [`arc/config`](config.md). |
| `worker-brief.md` | The repository's standing worker instructions: the last section of every worker brief. `init` writes a default once and never overwrites it. |
| `plans/<id>.md` | One plan: its fields and the request text. |
| `tasks/<id>.md` | One task: its fields and the brief. |
| `ledger/plans/<id>.json` | The `state_version` and SHA-256 arc last wrote for that plan. |
| `ledger/plans/<id>.last.md` | The plan as arc last wrote it, for `adopt`. |
| `ledger/tasks/<id>.json` | The same for a task. |
| `ledger/tasks/<id>.last.md` | The task as arc last wrote it. |
| `runs/<task>/attempts/<attempt id>/` | One attempt's output: the worker's `result.jsonl`, `stderr.log`, `execution.json`, the wrapper's `attempt_finished.json`, `signal.json` when the wrapper signalled the worker, `before/<n>.log` for each `before_attempt` command, arc's `attempt_result.json` (the class, cost, state, and a model wait's `model_error` with `model_wait`), and `paused.json` when the automatic rework limit paused the plan. |
| `runs/<task>/gate.json` | The background gate's record. Its `status` is one of `started`, `running`, `done`, `refused`, `error`, `no_record`, `died` and `interrupted`: `started` while a pass has started the gate but the process has not written its own record yet, `running` while it runs, then `done`, `refused` or `error`; `no_record` when the process exited without writing any record while the pass had planned the gate; `died` when a start died more often than [`limits.max_gate_deaths`](config.md#configtoml) allows for one attempt and one set of inputs; `interrupted` when the scheduling loop stopped it itself. It keeps the attempt it runs for, the inputs the pass planned for the gate (`started`, `refused`, `no_record` and `died`; absent when the pass could not plan them), and, across restarts of a dying gate, its `deaths`. |
| `runs/<task>/gate.log` | The background gate process's own output. |
| `runs/<task>/gate.json.lock` | The lock a pass and the gate process take around a write or removal of `gate.json`. A pass reads the record under it before it names the process it started, so that record is never written back over the gate's `running` one, and a record a pass removes is not put back by a holder that already read it. |
| `runs/<task>/auto_reverify.json` | The `(head_sha, remote target branch)` pairs a scheduling pass already re-verified for the task, and the text of the last `limits.max_auto_reverify` stop. |
| `runs/<task>/conflict_rework.json` | How many conflict reworks arc has written for the task in a row. The lead's rework and an attempt that does not end in a conflict set it back to `0`. |
| `runs/<task>/review_cost.json` | The attempt whose `review` stage cost arc already added to the task's counters, with the `cost_after` and `total_after` counters that charge produced. A pass that dies between the charge and the state move therefore neither loses nor doubles the review: an application whose task counters do not carry the recorded charge adds it again. |
| `runs/<task>/hooks/<point>-NN.log` | One hook command's stdout and stderr, where `NN` is the command's position in the point's list from 1 (`after_merge-01.log`). A point with `[]`, or one whose commands never started, has no file. |
| `runs/<task>/attempts/<attempt id>/verdicts/` | One gate verdict per stage (`commands.json`, `human.json`, `integration.json`, …), the last verify/merge reason (`review.json`), and an owner's reject note (`reject.md`). |
| `runs/<task>/attempts/<attempt id>/profile.json` | The gate profile that attempt's verification ran under: its name, stages, `before_attempt` and `commands`. A merge of that attempt reads it, so a flow removed or a [`gate_profile`](config.md#configtoml) changed after the gate ran does not change how its verdicts are approved and integrated. Its old `key_commands` still reads, but integration uses the current [`integration.key_commands`](config.md#configtoml), never it. A verdict written before the file existed leaves it absent, and the merge resolves the flow's profile then. |
| `runs/<task>/attempts/<attempt id>/logs/` | One log per gate command (`commands-01.log`, `integration-01.log`, …). |
| `runs/<task>/attempts/<attempt id>/merge-command.log` | The stdout and stderr of the [`integration.command`](config.md#configtoml) merge command; each run of the command overwrites it. |
| `attempts/<task>/…` | The host-local attempt records: `base_sha`, `last_head`, and per attempt `start.json`, `identity.json`, `status.json`, `heartbeat.json`. |
| `attempts/<task>/state/` | The directory a command worker receives as [`ARC_STATE_DIR`](worker.md#another-agent-as-the-worker): outside the worktree, kept across the task's attempts. The command may keep its own state there. |
| `gates/<stage>/` | One checkout per gate stage; the review stage uses one per concurrency slot, `gates/review-<n>/`. |
| `gates/review-<n>.lock` | One review slot's lock. |
| `gates/<task>.gate.lock` | The lock one task's running gate holds. |
| `worktrees/<task>/` | The worker's checkout for that task (the worker wrapper creates it). |
| `state.lock` | The short lock every change takes. |
| `runner.lock` | The lock a scheduling loop (`radian arc run` or `radian arc serve`) holds; its holder writes its pid there, which a viewer's header reads. |
| `loop.log` | Every line `radian arc run` or `radian arc serve` prints, each prefixed with an RFC 3339 UTC timestamp and a space, flushed per line: the transition lines, the summary, and the `radian arc:` error lines. The arc TUI's ticker and log view read it. A log that reaches 1 MiB is renamed to `loop.log.1` before the next line. |
| `loop.log.1` | The previous `loop.log`, kept when the log rotated. One old file is kept. |

Every change is one transaction under `state.lock`:
take the lock (waiting up to 5 seconds, then exit `4`), compare the file with the ledger, write a temporary file, flush it to disk, rename it over the old one, flush the directory, record the new version and hash in the ledger, release.

Search skips `worktrees/` and `gates/`: `grep`, `glob`, skill discovery, and `@` completion do not enter them.
Both are full checkouts of the tree, so searching them would return every file once per task and once per gate stage.
They skipped `.radian-worktrees/` for the same reason.
A worker never reads the rest of `.radian/arc/` from inside its checkout: the worker wrapper denies every entry except `worktrees/`.


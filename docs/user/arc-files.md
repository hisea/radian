---
description: The files arc keeps: config.toml, plan and task files, and the .radian/arc/ layout.
---

# arc: the files

This page covers the files arc keeps; the commands are in [`arc`](arc.md), the scheduling loop is in [`arc-loop`](arc-loop.md), and the gate and its failures are in [`arc-gate`](arc-gate.md).

## `config.toml`

`init` writes `config.toml` and every other subcommand reads it.
Unknown keys are refused by name, and `schema_version` must be `1`.
A key you leave out takes its default.

| Key | Type | Default | Allowed values | When it takes effect |
|---|---|---|---|---|
| `schema_version` | integer | `1` | `1` | Every run; another value is refused. |
| `repository.remote` | string | `"origin"` | A remote name that exists in the repository | When arc reads the main branch or pushes a task branch. |
| `repository.main_branch` | string | `"main"` (`init` writes what it found) | A branch name | When a task starts from main, and when main is checked before a merge. |
| `limits.concurrency` | integer | `1` | 1 or more | When `radian arc run` starts tasks: how many attempts may be in flight at once. A running gate takes no slot. |
| `limits.max_attempts` | integer | `3` | 1 or more | When a task fails and arc decides whether to retry: how many retries the task gets since its last lead or review rework. The count is the task's [`retry_count`](#task-fields); the attempt number is never compared with the limit, and a conflict rework keeps the count. |
| `limits.stall_timeout_secs` | integer | `900` | seconds | When a worker prints no new line for that long. |
| `limits.max_wall_time_secs` | integer | `7200` | seconds | As an absolute limit on one attempt, on top of the stall timeout. |
| `limits.git_remote_timeout_secs` | integer | `300` | seconds, `1` or more | A git call that talks to a remote (`fetch`, `push`, `ls-remote`): the call, and everything it started, is killed when it runs this long. It takes effect at the next git call. A local git call (`diff`, `log`, `merge-base`, `status`, a local `merge`) is not bounded. In the worker wrapper the shorter of this and the attempt's remaining wall time applies. |
| `limits.max_cost_usd` | number | not set: no limit | 0 or more, US dollars | When an attempt ends and when a `review` verdict is applied: every worker attempt and every `review` stage adds its cost, and the task or plan budget that is reached pauses the task. A `review` run that reports no cost adds `0`. |
| `limits.cancel_grace_secs` | integer | `10` | seconds | After a cancel, before the worker gets SIGKILL. |
| `limits.max_auto_rework` | integer | `2` | 0 or more | When an automatic rework would go past the limit; the plan then pauses. A review rework counts; a conflict rework that arc writes itself does not. |
| `limits.max_conflict_rework` | integer | `3` | 0 or more | When a verification or integration stage fails with a conflict with main: how many conflict reworks arc writes in a row for one task. While it has written fewer, the conflict gets the brief; once it has written the limit, arc writes no brief, the task stays in `review.rework_pending` and waits for you. The lead's rework and an attempt that does not end in a conflict set the run back to `0`. `0` writes none, so the first conflict waits for you; `1` writes exactly one. |
| `limits.max_auto_reverify` | integer | `2` | 0 or more; `0` turns the feature off | When a `commands` or integration command failure waits for a moved main: how many automatic reverifies one task gets. A hand `radian arc reverify` neither checks the count nor uses it up. |
| `limits.max_gate_deaths` | integer | `2` | 0 or more; `0` stops at the first death | When a background gate wrote `running` and then died for the same attempt and inputs: how many deaths it gets before the record becomes `died` and no gate starts for those inputs. The gate is started `1 + max_gate_deaths` times at most. A gate the scheduling loop stopped itself is `interrupted`, not a death (see [the gate run](arc-gate.md#the-gate-run)). |
| `worker.exec_args` | array of strings | `[]` | any `radian exec` argument except `--profile`, `--workspace`, `--session-id`, `--resume`, `--output`, `--yes`, `-p` | Before `start`, each `run` pass and `confirm` act, the line the worker would get is parsed with the `radian exec` parser; a rejected line exits `3`. Every attempt: the arguments go to the worker after the wrapper's fixed ones. Empty means the worker uses the host's normal model configuration. |
| `worker.rework_session` | string | `"fresh"` | `fresh`, `resume` | The session each worker attempt runs in. `fresh`: every attempt, a retry or a rework, starts a session of its own, `<task>-<attempt>`, and an attempt after the first gets a hand-off section in front of the brief ([what a rework receives](arc.md#radian-arc-start-task)). `resume`: one session per task, whose id is the task id, created by the first attempt that reaches the model and continued by every later one. A change takes effect at the next attempt. |
| `ids.plan_prefix` | string | `"PL"` | 1 to 4 ASCII uppercase letters; must differ from `ids.task_prefix` | When a plan id is created. |
| `ids.task_prefix` | string | `"TK"` | 1 to 4 ASCII uppercase letters; must differ from `ids.plan_prefix` | When a task id is created. |
| `gate.profile` | string | `"default"` | The name of one `[gate.profiles.<name>]` section | When a gate runs. |
| `gate.command_timeout_secs` | integer | `3600` | seconds | Per gate command line. |
| `gate.review.exec_args` | array of strings | not set: `[worker] exec_args` | any `radian exec` argument except `--tools`, `--session-id`, `--workspace`, `--profile`, `--resume`, `--deny-read`, `--mcp-config`, `--yes`, `--output`, `--no-config-mcp`, `-p` | The review session's model route, after the stage's own arguments. Every review run except a first review's when `gate.review.first.exec_args` is set. |
| `gate.review.concurrency` | integer | `2` | `1` or more | When the review stage starts: how many review sessions may run at once in the repository. Each session holds one of the locks `gates/review-0.lock` … `gates/review-<n-1>.lock` and its own checkout, `gates/review-<n>/`; `1` runs one review at a time. A change takes effect at the next review. |
| `gate.review.first` | table | empty | `exec_args` and `timeout_secs`, each optional | A task's first review, a review stage run when no earlier attempt of the task has a `review` verdict. Each key set here replaces the same `[gate.review]` key for that review; a rework review never reads it. A change takes effect at the next first review. |
| `gate.review.first.exec_args` | array of strings | not set: `gate.review.exec_args` (and then `worker.exec_args`) | any `radian exec` argument except `--tools`, `--session-id`, `--workspace`, `--profile`, `--resume`, `--deny-read`, `--mcp-config`, `--yes`, `--output`, `--no-config-mcp`, `-p` | The first review session's model route, after the stage's own arguments. Unset, the first review uses `gate.review.exec_args`. |
| `gate.review.first.timeout_secs` | integer | not set: `gate.review.timeout_secs` | seconds; `1` or more | The wall-time limit of one first-review session. Unset, the first review uses `gate.review.timeout_secs`. |
| `gate.review.prompt_file` | string | not set: only the built-in prompt | a path relative to the repository root, without `..` | Appended to the built-in review prompt when the `review` stage runs. |
| `gate.review.timeout_secs` | integer | `900` | seconds; `0` waits forever | The wall-time limit of one review session. |
| `gate.review.propose_followups` | boolean | `true` | `true`, `false` | When a task merges: `true` lets arc add one `todo.proposed` task with the items every review round of the task suggested but the merge left unfixed (see [The review's follow-up](arc-gate.md#the-reviews-follow-up)); `false` turns it off. The lead's MCP `add_task` on a confirmed plan is not affected. |
| `gate.profiles` | table | one `default` section | One section per profile name | When a gate runs. |
| `gate.profiles.<name>.stages` | array of tables | `[{ stage = "commands", required = true }, { stage = "human", required = true }]` | `stage` is `commands`, `mutation`, `review`, `ci`, or `human`; `required` is `true` or `false` and defaults to `true` | When a gate runs. The list's order is the round's order; when it lists both `commands` and `review`, those two start at the same time and the round is decided in the list's order ([the gate run](arc-gate.md#the-gate-run)). `integration` is not a profile stage: it always runs at merge. |
| `gate.profiles.<name>.commands` | array of strings | `[]` | Shell command lines | The `commands` stage runs them in order on its own checkout. |
| `gate.profiles.<name>.before_attempt` | array of strings | `["merge-main"]` | `merge-main`, or `[]` for no step | Before each attempt that reuses the worktree. `merge-main` merges the remote main into the branch. |
| `gate.profiles.<name>.key_commands` | array of strings | `[]` | Shell command lines | The integration stage runs them on the merge candidate. |

`gate.profiles.<name>` above is written out as `gate.profiles.default` in the file `init` writes.
A stage may be listed once per profile: a profile that lists a stage twice, or that lists `integration`, is refused by name.
A per-task field of the same name overrides a `[limits]` value for that task; a plan budget adds to the task budgets.

An attempt's worker session id is decided by its claim, which writes it to `attempts/<task>/<attempt>/start.json`; the wrapper writes the session it used to `identity.json` when it spawns the worker and to `execution.json` when it ends.
A reader takes the first of those records that names one: `execution.json`, then `identity.json`, then `start.json`.
With `worker.rework_session = "fresh"` it is `<task>-<attempt>`; with `resume` it is the task id.
`radian arc show` prints it as `session_id` (known from the claim on, not only after the attempt ends), and `radian attach <session id>` opens that session's journal.

Every review run of an attempt has a session of its own, named `<attempt id>-review-<run>`.
When a journal of that name already exists (a gate was started again for the same attempt), the stage uses `<attempt id>-review-<run>-<k>` with the smallest free `k` and writes the run's logs as `review-<run>-<k>.jsonl` and `review-<run>-<k>.stderr.log` under the attempt's `logs/`; the review verdict's `review_session_ids` lists the ids it used.

## `worker-brief.md`

`<repository>/.radian/arc/worker-brief.md` is the repository's standing worker instructions.
It is optional.
[`radian arc init`](arc.md#radian-arc-init-track) writes it with this default when the file does not exist, and never overwrites a file you edited:

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

A file over 32 KiB (32,768 bytes), a file that is not UTF-8, a path that is not a regular file (a FIFO, a directory, a device), and a read error are not appended: the attempt runs without the section, its record carries one line `worker-brief.md skipped: <reason>`, and [`radian arc show`](arc.md#radian-arc-show-id-json) prints it as `attempt_note`.
The worker never reads the file itself: its sandbox denies `.radian/arc/`, and the wrapper puts the content on the worker's stdin.

## Plan and task files

A plan file is `plans/<id>.md`; a task file is `tasks/<id>.md`.
Both are front matter between two `---` lines, then a body.
arc is the only writer; you change them with [the `radian arc` subcommands](arc.md), and `adopt` after a hand edit.

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
`priority` is written only once you set it with [`radian arc priority`](arc.md#radian-arc-priority-plan-priority); until then the field is absent and the plan reads as `P3`, so an older plan file is written back unchanged.

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
| `supersedes` | string | no | the id of a task this one replaces |
| `attempt` | integer | yes | 0 or more; `0` when the task is new |
| `attempt_id` | string | no | the current attempt's `<task>-<n>`, written by the claim |
| `token` | string | no | the current attempt's random token; a result is applied only when it matches |
| `cost_usd` | number | no | the task's counted cost in US dollars: every worker attempt and every `review` stage, summed; the budget compares this number, and `reset-budget` makes it `0` |
| `total_cost_usd` | number | no | the task's cost in US dollars: every worker attempt and every `review` stage, summed; `reset-budget` does not change it. A file without the field reads its `cost_usd` as the total until its next write. |
| `auto_rework` | integer | no | `0` or more; how many reworks arc counted while the plan's `auto` was not `off`; `0` when absent |
| `retry_count` | integer | no | `0` or more; how many retries arc started since the task's last lead or review rework; `0` when absent. The lead's brief and the review rework set it back to `0`; a conflict rework keeps it, and `max_attempts` counts it. |
| `allow_noop` | boolean | yes | `true`, `false`; `false` when the task is new |
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
A `todo` / `retry_wait` task is claimed at once by `radian arc start`, without waiting for its `retry_at`; the scheduling loop waits for `retry_at` (see [`radian arc start`](arc.md#radian-arc-start-task)).
A task the lead adds to a confirmed plan (MCP `add_task`) or arc proposes from a merged task's review is `todo` / `proposed`: the scheduling loop never claims it and `start` refuses it until you run `radian arc confirm <task>`.
A plan is done when all of its tasks are merged; `done` is not a stored plan status.
A `cancelled` plan is final: `radian arc cancel <plan>` sets it after ending every open task of the plan (see [`radian arc cancel`](arc.md#radian-arc-cancel-id)).
No command confirms it, adds a task to it, changes its `auto` or `priority`, resets its budget, or starts, reverifies or gates its tasks, and `list` hides it unless `--all`.

### The review states

A task leaves `doing` for `review.verifying` when a pushed attempt ends.

| From | To | What moves it |
|---|---|---|
| `review.verifying` | `review.awaiting_human` | Every required non-`human` stage passed for `verified_sha`; `verified_sha` and `verdict_set_digest` are recorded. |
| `review.verifying` | `review.rework_pending` | A stage failed: the stage, its reason and the verdict are recorded. |
| `review.awaiting_human` | `review.integrating` | `radian arc merge`, or `radian arc run` on a plan at `auto` `merge`, with `verified_sha` still on the remote, the digest unchanged, and every required stage passed. |
| `review.awaiting_human` | `review.rework_pending` | `radian arc reject --note <text>`: the note is recorded. |
| `review.integrating` | `review.ready_to_merge` | The integration stage passed against the current main; its `base_sha` and `candidate_sha` are in the integration verdict. |
| `review.integrating` | `review.rework_pending` | A conflict, or a failing key command. |
| `review.ready_to_merge` | `done.merged` | Main is still the integration `base_sha`; the candidate was pushed to the remote main and contains `verified_sha`; the worktree is removed. |
| `review.ready_to_merge` | `review.integrating` | Main moved after integration, or the push was rejected as non-fast-forward: integrate again. |
| `review.awaiting_human` | `review.verifying` | The remote `work/<task>` is no longer `verified_sha`, or the verdict set no longer matches the recorded digest: every verdict and the approval of the old commit are invalid. |
| `review.rework_pending`, `review.awaiting_human` | `review.verifying` | `radian arc reverify`, or `radian arc run` reverifying a `commands` or integration command failure by itself once main moved: a new attempt is claimed without a worker; its verdicts are its own. |
| `review.verifying` | `review.awaiting_human` (again) | `radian arc run` or `radian arc start` re-verifies a task a moved branch or a changed digest sent back. |
| `review` (any phase) | `todo.ready` | The lead adds a rework brief (`add_rework`); the next claim starts the next attempt. |
| `review` (any phase) | `failed.cancelled` | `radian arc cancel <task>`; a running gate is signalled first. |
| `todo.proposed` | `todo.ready` | `radian arc confirm <task>`: the task is confirmed, and its plan's `status` and `auto` do not change. |
| `todo.proposed` | `failed.cancelled` | `radian arc cancel <task>`. |

## The `.radian/arc/` layout

Everything arc stores for one repository is in `<repository>/.radian/arc/`:

| Path | What it holds |
|---|---|
| `config.toml` | The settings above. |
| `worker-brief.md` | The repository's standing worker instructions: the last section of every worker brief. `init` writes a default once and never overwrites it. |
| `plans/<id>.md` | One plan: its fields and the request text. |
| `tasks/<id>.md` | One task: its fields and the brief. |
| `ledger/plans/<id>.json` | The `state_version` and SHA-256 arc last wrote for that plan. |
| `ledger/plans/<id>.last.md` | The plan as arc last wrote it, for `adopt`. |
| `ledger/tasks/<id>.json` | The same for a task. |
| `ledger/tasks/<id>.last.md` | The task as arc last wrote it. |
| `runs/<task>/attempts/<attempt id>/` | One attempt's output: the worker's `result.jsonl`, `stderr.log`, `execution.json`, the wrapper's `attempt_finished.json`, `signal.json` when the wrapper signalled the worker, arc's `attempt_result.json`, and `paused.json` when the automatic rework limit paused the plan. |
| `runs/<task>/gate.json` | The background gate's record. Its `status` is one of `started`, `running`, `done`, `refused`, `error`, `no_record`, `died` and `interrupted`: `started` while a pass has started the gate but the process has not written its own record yet, `running` while it runs, then `done`, `refused` or `error`; `no_record` when the process exited without writing any record; `died` when it wrote `running` and died more often than [`limits.max_gate_deaths`](arc-files.md#configtoml) allows for one attempt and one set of inputs; `interrupted` when the scheduling loop stopped it itself. It keeps the attempt it runs for, the inputs the gate was started with (`started`, `refused`, `no_record` and `died`), and, across restarts of a dying gate, its `deaths`. |
| `runs/<task>/gate.log` | The background gate process's own output. |
| `runs/<task>/auto_reverify.json` | The `(head_sha, remote main)` pairs a scheduling pass already re-verified for the task, and the text of the last `limits.max_auto_reverify` stop. |
| `runs/<task>/conflict_rework.json` | How many conflict reworks arc has written for the task in a row. The lead's rework and an attempt that does not end in a conflict set it back to `0`. |
| `runs/<task>/attempts/<attempt id>/verdicts/` | One gate verdict per stage (`commands.json`, `human.json`, `integration.json`, …), the last verify/merge reason (`review.json`), and an owner's reject note (`reject.md`). |
| `runs/<task>/attempts/<attempt id>/logs/` | One log per gate command (`commands-01.log`, `integration-01.log`, …). |
| `attempts/<task>/…` | The host-local attempt records: `base_sha`, `last_head`, and per attempt `start.json`, `identity.json`, `status.json`, `heartbeat.json`. |
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

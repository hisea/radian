---
description: The gate's review stage: its prompt and verdict, the rework round, a failed review, the follow-up and a wait on an unavailable model.
---

# arc: the review stage

The gate's `review` stage.

## The review stage

- `review` is a read-only radian session, not the worker session.
  It runs at the same time as `commands` when the profile lists both and before `human`; a profile that lists both must list `commands` first.
  For each run it starts one `radian exec` in its own slot checkout, `.radian/arc/gates/review-<n>/`, on the candidate, with the candidate as the workspace and only the `read`, `grep` and `glob` tools mounted: no shell tool, no write tool, and no MCP server.
  Its model route is `[gate.review] exec_args`, and `[gate.review] timeout_secs` limits one session.
  A task's **first review** is a review stage run when no earlier attempt of the task has a `review` verdict.
  It reads `[gate.review.first] exec_args` and `[gate.review.first] timeout_secs` when they are set; an unset key falls back to the same `[gate.review]` key and, for `exec_args`, then to `worker.exec_args`.
  A rework review never reads `[gate.review.first]`.
  Both keys are read when the stage starts, so a change takes effect at the next review.
  The prompt is built in: the task's brief, the commit list and the diff from the merge base of the remote target branch and `verified_sha` to `verified_sha` (the task's own change, not the target branch's), and the instruction to check the change against the brief and the repository's rules (`AGENTS.md` and the design records the brief names).
  A rework round's prompt holds the increment instead of the full diff (see [A rework round](#a-rework-round)).
  `[gate.review] prompt_file` is appended to it.
  The session must end with one fenced `json` block, and arc parses the last such block:

  ```json
  {"summary": "what the change does", "blocking": [{"level": "major", "file": "path", "line": 12, "text": "what is wrong"}], "suggestions": [], "verified": []}
  ```

  `summary` is 2 to 6 sentences. Each item of `blocking` and `suggestions` is an object: `text` is required, and `level`, `file` and `line` are optional. `level` is one of `critical`, `major`, `minor`. A plain string is an item with only its `text`, so a verdict written before the object form still reads.
  `blocking` lists what must change before the change can merge; an empty list passes the stage. `suggestions` and `verified` never fail the stage; the suggestions of the review rounds of a plan's merged tasks become the plan's follow-up task once the plan has no open task (see [The review's follow-up](#the-reviews-follow-up)).
  A blocking item fails the stage with reason `review_blocking`; the task goes to `review.rework_pending` and `rework_reason` lists the items, each as `[level] path:line text` with the parts an item does not have left out. An item whose `text` starts with a tag that repeats its own level (`[major]`, any case, spaces allowed) prints the level once: the leading tag is dropped from the text. A `text` that starts with another tag keeps it, and the verdict on disk is never rewritten.
  No parsable block, a missing field, a `level` outside the three values, a timeout, or a session error runs the session once more; when the second run is unusable too the stage fails with reason `review_output`, or `review_timeout` when the last run timed out. Both runs' session ids and their summed cost are in the verdict.
  A session whose `Result` carries a `model_error` with `kind` `unavailable` or `usage_limit` is **model unavailable**: the stage does not run it a second time, fails at once with reason `review_unavailable`, and its verdict records the session id, its summed cost and that `model_error` object as `review_model_error` (see [A review wait on an unavailable model](#a-review-wait-on-an-unavailable-model)).
  A gate that is started again for the same attempt does not reuse a review session whose journal exists: the stage names the run `<attempt id>-review-<run>-<k>` with the smallest free `k`, writes its logs as `review-<run>-<k>.jsonl` and `review-<run>-<k>.stderr.log`, and `review_session_ids` lists the ids it used, so every run's session is found.
  The review verdict is `runs/<task>/attempts/<attempt id>/verdicts/review.json`; it records `review` (the four fields), `review_session_ids`, `review_cost_micros`, `review_model_error` (only on a `review_unavailable` failure), `review_settings` (`round`, `first` or `rework`; the `exec_args` source key; `timeout_secs` and the key it came from) and, in a rework round, `review_pre_existing` (how many items arc moved), beside the usual `sha`, `main_sha`, `candidate_sha` and `profile`. It is bound to `verified_sha`, is part of `verdict_set_digest`, and `reverify` runs it again.
- `review` failure reasons: `review_blocking` (with its items in `rework_reason`), `review_output` (no usable block after two runs), `review_timeout` (the last run hit `[gate.review] timeout_secs`), and `review_unavailable` (the session's model was unavailable; see [A review wait on an unavailable model](#a-review-wait-on-an-unavailable-model)).
- When the profile lists both `commands` and `review`, the round's outcome follows the profile order.
  A `commands` failure decides it with the commands reason, even when the review passed or failed too: the review verdict is still written, its cost counts, `show` lists its items, and arc writes no automatic review rework for that round.
  When `commands` passes and `review` fails, the review path runs (see [A failed review](#a-failed-review)).
  Both verdicts are written when each stage ends, and a verdict already written is reused: a gate that died while the pair ran resumes only the stage without a verdict.
  The gate waits for both stages to end before it decides; a failing stage does not kill the other, and the review is bounded by its own limit.
  A stop signal to the gate reaches both running stages.

### A rework round

A review is a rework round when an earlier attempt of the same task has a review verdict for a `sha` other than the commit under review; the newest such verdict is the previous review.
A review of the same commit as the previous review (a `reverify` of an unchanged head) gets the first review's prompt: the full diff, and no item is pre-existing.
It reads the `[gate.review]` settings, not `[gate.review.first]`, because an earlier review verdict exists, so it is not a first review.
A genuine first review, one with no earlier verdict at all, keeps the full-diff prompt and the `[gate.review.first]` settings.

A rework round's prompt holds the brief, the previous review's blocking items, the commit list `<previous sha>..<sha>`, and the increment instead of the full diff.
It asks the session to check that each previous blocking item is fixed, to review the increment, and to list every problem it finds; the session can read the files.

The increment is the task's own diff restricted to the hunks that hold a line the rework changed.
A line is an increment line when it is a new-side line of `git diff <previous sha> <sha>` and also a new-side line of the task's own diff, which starts at the merge base of the remote target branch and `sha`.
A line that came in only with a merge of main is not in the task's own diff, so it drops out.

arc, not the session, decides whether a blocking item is pre-existing.
After a rework round's report is parsed, an item is moved to `suggestions` and marked `"pre_existing": true` unless one of these holds:

- its `file:line` is within 3 lines of a line the task's own change added or changed in that file (the new side of `git diff <the merge base of the remote target branch and the head>..<head>`): a problem in code the task itself wrote in an earlier attempt is the task's, not pre-existing, so the rework's increment does not decide this;
- it is in the same file as and within 10 lines of a blocking item of the previous review, so an item that was not fixed stays blocking;
- its `level` is `critical`: a `critical` item blocks in every round, wherever it is;
- it has no `file` or no `line`: it stays in `blocking` at its level, because the review prompt asks for both fields and an omission is not evidence that the problem is old.

A moved item keeps its `level` (`major` included), and a `major` item in a file the task's own change does not touch, or more than 3 lines from any changed line in that file, is still moved, so the rework is not sent back for a problem that predates the task; `minor` items follow the same window rules.
The stage passes when no blocking item is left, and the verdict records how many items arc moved as `review_pre_existing`.
The follow-up task of a plan carries a moved item at its level (see [The review's follow-up](#the-reviews-follow-up)).

When a rework round's review still fails with blocking items and the plan's `auto` is not `off`, arc writes the next attempt's brief itself (see [A failed review](#a-failed-review)).

### A failed review

A review that failed with `review_blocking` returns the task to `todo.ready` with a brief arc writes, the way a conflict with main does, when the plan's `auto` is not `off`.
The heading is `## Rework <n> (review)`, with the lead's `add_rework` numbering.
The text says the review found these items, to fix them and nothing else, to append commits and not to rebase; it lists the blocking items, one per line, as `[level] path:line text`; and it names the review verdict to read for detail.
The worker cannot read `runs/`, so the wrapper copies the verdict into the worker's worktree at `.radian/arc-verdicts/<task>/<attempt>/<file>` and puts that worktree-relative path in the brief; the task file keeps the absolute path. A verdict that is missing, is not a regular file, is larger than 1 MiB, or cannot be copied stays in the brief as it is, and the attempt record carries `verdict copy skipped: <path>: <reason>`, which [`radian arc show`](planning.md#radian-arc-show-id-json) prints as `attempt_note`.
It counts as an automatic rework: it raises `auto_rework`, and `max_auto_rework` pauses the plan (see the [`limits.max_auto_rework` row](config.md#configtoml)); the task then stays in `review.rework_pending`.
With `auto` `off`, the task stays in `review.rework_pending` and waits for you, as before.
`radian arc run` prints `<task> review blocking -> todo.ready (automatic rework)`.

### The review's follow-up

A task is **open** when it is in `todo.ready`, `doing.*` or `review.*`; a `todo.proposed`, `done.*` or `failed.*` task is not open.
When a plan has no open task, arc collects the review suggestions of the merged tasks that merged under this rule and proposes one follow-up task for the plan, and no more.
arc checks after every change that can leave a plan with no open task: a merge, a `cancel`, and every scheduling pass.
The check is idempotent: running it again adds no task.
The check and the write are one transaction under `state.lock`: the open-task condition and the set of merged tasks to collect are re-read under the lock that appends the follow-up, so a `confirm` between a scheduling pass's read and its write cannot add a follow-up beside an open task.

When a task reaches `done.merged`, arc records `followup_pending` on it if [`gate.review.propose_followups`](config.md#configtoml) is `true` at that moment and the task is not itself a follow-up.
That is the per-plan rule: every merge path writes the field under the same switch the follow-up check reads.
A merged task is **collected** only when it carries that field, is not itself a follow-up, and no earlier follow-up of the plan names it in its `after`.
A follow-up's `after` lists the merged tasks it collected, so a task is collected at most once, and a collected task whose follow-up is later cancelled stays collected.
A merged follow-up is never collected.

A task merged before the field existed (by an older arc), or merged while `propose_followups` was `false`, does not carry it and is never collected, whatever its review verdicts say: turning the switch back on does not bring it back.
So an old per-task follow-up (the title `Follow-ups from <task id>'s review`, with that task as its only `after` entry) collects nothing, and it is never collected itself.

For each collected task arc reads every `review` verdict of the task, oldest attempt first, and collects every `suggestions` item of every round; the items a rework round moved (marked `pre_existing`) are in `suggestions`.
Every round is read, not only the newest: a round that failed can carry a demoted item, and a later round can pass with nothing to suggest.
Nothing from `blocking` is collected: a merged task's blocking items were fixed or the review would not have passed, so a round that only had blocking items contributes nothing.

Two items of the same merged task are the same when their `file`, `line` and `text` are equal after a leading tag that repeats the item's own level is removed and whitespace is collapsed; arc keeps the newest copy.
An item with the same `file:line` and other text is kept as its own item.
Items of different tasks are never merged.

When at least one item is left, and `[gate.review] propose_followups` is `true` (the default), arc adds one task to the plan, and no more:

- the id is the next task id;
- the slug is `<plan slug>-followups`, cut to the 48-character slug limit with a trailing `-` removed; when the plan already holds a task with that slug, `-2`, `-3`, … is appended within the limit;
- the title is `Follow-ups from <plan id>'s reviews`;
- its `after` is the collected merged tasks, in merge order;
- its phase is `todo.proposed`, so the loop never claims it until you confirm it (see [`confirm`](owner.md#radian-arc-confirm-id));
- its `allow_noop` is `true`: its brief lets each item be left with a one-line reason, so an attempt that adds no commit may deliver nothing (see [An empty change](gate.md#an-empty-change));

Its brief is fixed: one line saying the items are from the reviews of the plan's merged tasks and that each item is to be fixed or left with a one-line reason in the final answer; then one section per collected task, headed `From <task id>`, holding that task's items grouped by the round they came from, each group headed `From review <n> (attempt <n>):`, one item per line as `[level] path:line text`; then the path of every review verdict.
The worker gets each verdict as a copy at `.radian/arc-verdicts/<task>/<attempt>/<file>` in its worktree, one copy for every task and attempt the brief names, and the task file keeps the absolute paths; the skip line is the same as for a review rework ([A failed review](#a-failed-review)).
An item whose `text` starts with a tag that repeats its own level prints the level once, as in the rework reason above.

A plan whose merged tasks left no item gets no follow-up.
A follow-up task gets no follow-up of its own: a merged follow-up is never collected, so the review's unfixed items stay in its verdict and in the task's final report.
arc recognises a follow-up by a shape it gave it: the per-plan shape above (title `Follow-ups from <plan id>'s reviews`, the `<plan slug>-followups` id, `allow_noop`, a non-empty `after` list, and the fixed first line of its brief), or the per-task shape an earlier arc wrote (title `Follow-ups from <task id>'s review` with that task as its only `after` entry).
With `propose_followups` `false` arc adds none (see the [`propose_followups` row](config.md#configtoml)); the lead can still add a task to the plan with MCP `add_task`.
`radian arc run` prints `<new task id> proposed (follow-ups from <plan id>)`.

A follow-up arc cannot add (the plan's file changed outside arc) does not undo the merge: `radian arc merge` prints it as a `warning:` line after the merge, and `radian arc run` reports it in the summary.
The plan keeps the state it had.

### A review wait on an unavailable model

A `review` stage whose session ended because its model was unavailable fails with `review_unavailable` (see [the review stage](#the-review-stage)) and the task waits in `review.rework_pending`. Its `rework_reason` is `review_unavailable: model <kind> (HTTP <status>), retry at <UTC time>`, with the parts that are null left out. arc writes no rework section, does not raise `auto_rework`, does not touch `retry_count`, and does not pause the plan; the wait counts the review's cost toward the budget as any review's does.

The wait is the task's `retry_at`: the failing session's `model_error.retry_after_secs` when it is present, at least 60 s and at most 6 hours; otherwise 5 minutes for the first `review_unavailable` of a `verified_sha`, doubled for each further one in a row on the same `verified_sha`, at most 60 minutes. Any other review outcome ends the run of waits.

A scheduling pass reverifies the task by itself once `retry_at` has passed, the way `radian arc reverify` does: it claims a new attempt without a worker and starts its gate. It applies when the task's plan has `auto` other than `off`. It needs no moved main, does not use up [`limits.max_auto_reverify`](config.md#configtoml), and has no count limit, because a usage window always resets. The claim is not a retry and not an automatic rework: it changes neither `retry_count` nor `auto_rework`, and it does not pause the plan. Your own `radian arc reverify` may start it earlier, as for any failed verification stage.

`radian arc run` prints `<task> review model unavailable -> retry at <UTC time>` when the stage fails and `<task> review retry_at passed -> auto reverify` when it starts the reverify ([the loop's lines](loop.md#radian-arc-run)). `radian arc show` prints the reason as `rework_reason` and the wait as `retry_at`, and `radian arc list` ends the task line with `retry at <UTC time>`.

A worker attempt that ends on an unavailable model waits like the review stage, and uses no retry (see [A worker wait on an unavailable model](gate-failures.md#a-worker-wait-on-an-unavailable-model)).


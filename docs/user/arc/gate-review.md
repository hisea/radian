---
description: The gate's review stage: its prompt and verdict, the rework round, a failed review, the follow-up and a wait on an unavailable model.
---

# arc: the review stage

The gate's `review` stage.

## The review stage

- `review` is a read-only radian session, not the worker session.
  It runs at the same time as `commands` when the profile lists both and before `human`; a profile that lists both must list `commands` first.
  For each run it starts one `radian exec` in its own slot checkout, `.radian/arc/gates/review-<n>/`, on the candidate, with the candidate as the workspace and only the `read`, `grep` and `glob` tools mounted: no shell tool, no write tool, and no MCP server.
  Its model route and time limit come from the reviewer its gate profile names: the profile's [`reviewer`](config.md#configtoml) picks a `[gate.reviewers.<name>]` section, whose `exec_args` and `timeout_secs` the session uses.
  A task's **first review** is a review stage run when no earlier attempt of the task has a `review` verdict.
  It reads the reviewer's `first.exec_args` and `first.timeout_secs` when they are set; an unset key falls back to the reviewer's own key and, for `exec_args`, then to `worker.exec_args`.
  A rework review never reads the reviewer's `first` table.
  Both keys are read when the stage starts, so a change takes effect at the next review.
  The prompt is built in: the task's brief, the commit list and the diff from the merge base of the remote target branch and `verified_sha` to `verified_sha` (the task's own change, not the target branch's), and the instruction to check the change against the brief and the repository's rules (`AGENTS.md` and the design records the brief names).
  A rework round's prompt holds the increment instead of the full diff (see [A rework round](#a-rework-round)).
  The reviewer's `prompt_file` is appended to it.
  The session must end with one fenced `json` block, and arc parses the last such block:

  ```json
  {"summary": "what the change does", "findings": [{"severity": "major", "file": "path", "line": 12, "summary": "what is wrong", "trigger": "the input that shows it"}], "verified": []}
  ```

  `summary` is 2 to 6 sentences on what the change does. Each finding is an object: `severity` and `summary` are required, and `file`, `line` and `trigger` are given when they apply. An absent `trigger` reads as empty. `severity` is one of `critical`, `major`, `minor`. `trigger` is the input or state that shows the problem. `findings` lists every problem found, not only the first.
  The three severities are calibrated by these examples:

  - `critical`: security, data loss or corruption, a state the program cannot recover from, a hang or a leak. Examples: a cancel signals only the direct child, so a command the wrapper started in another process group keeps running after the task is cancelled; a file the code reads whole is replaced by a FIFO, and the read blocks forever before any timeout applies; a struct that holds an API key derives `Debug`, so a panic or an error path prints the key.
  - `major`: a wrong result, a contradiction of the brief or a design record, a manual sentence the code does not do, a test that fails or cannot fail. Examples: the test binary no longer compiles on Linux because a helper has no such method; a JSON `null` for a field deserializes as absent, so the documented "a present value that is not an array is an error" never fires; the manual says the flag is set only for a hook exit 2, but an exit 0 with a block decision also sets it; a new edge-case test passes against the code before the change, so it proves nothing; attempt directories are sorted by name, so attempt 10 sorts before attempt 2 and the newest verdict is the wrong one.
  - `minor`: everything else: a rare edge case, wording, naming, style. Examples: a pronoun in a doc sentence can refer to two nouns, say which; the summary row's indentation no longer aligns under the id column after a column was inserted; a comment describes a key-handler arm that an earlier handler now intercepts, so the arm is unreachable; a tool's schema still says "default: every plan" after the default changed.

  A finding at or above the reviewer's `fix_level` and not `pre_existing` fails the stage with reason `review_findings`. The task goes to `review.rework_pending` and `rework_reason` lists the failing findings, highest severity first, each as `[severity] path:line summary — trigger` with the parts a finding does not have left out; the ` — trigger` part goes when the trigger is empty. A finding below `fix_level`, and a `pre_existing` one, is recorded and does not fail the stage.
  `fix_level` is the reviewer's [`fix_level`](config.md#configtoml): `critical`, `major` (the default) or `minor`. `critical` sends back only critical findings, `major` sends back critical and major ones, and `minor` sends back every finding.
  `verified` never fails the stage.
  A verdict written before findings existed holds `blocking` and `suggestions`; it still reads back as findings: a `blocking` item at its own `level`, else `major`, a `suggestions` item at its own `level`, else `minor`, its `text` as the summary, and no trigger. A `text` that starts with a tag that repeats that level loses the tag. `show` prints the same items for such a verdict.
  No parsable block, a missing field, a `severity` outside the three values, a timeout, or a session error runs the session once more; when the second run is unusable too the stage fails with reason `review_output`, or `review_timeout` when the last run timed out. Both runs' session ids and their summed cost are in the verdict.
  A session whose `Result` carries a `model_error` with `kind` `unavailable` or `usage_limit` is **model unavailable**: the stage does not run it a second time, fails at once with reason `review_unavailable`, and its verdict records the session id, its summed cost and that `model_error` object as `review_model_error` (see [A review wait on an unavailable model](#a-review-wait-on-an-unavailable-model)).
  A gate that is started again for the same attempt does not reuse a review session whose journal exists: the stage names the run `<attempt id>-review-<run>-<k>` with the smallest free `k`, writes its logs as `review-<run>-<k>.jsonl` and `review-<run>-<k>.stderr.log`, and `review_session_ids` lists the ids it used, so every run's session is found.
  The review verdict is `runs/<task>/attempts/<attempt id>/verdicts/review.json`; it records `review` (`summary`, `findings` and `verified`), `review_session_ids`, `review_cost_micros`, `review_model_error` (only on a `review_unavailable` failure), `review_settings` (`round`, `first` or `rework`; `reviewer`, the section's name; `fix_level`; the `exec_args` source key; `timeout_secs` and the key it came from) and, in a rework round, `review_pre_existing` (how many findings arc marked), beside the usual `sha`, `main_sha`, `candidate_sha` and `profile`. It is bound to `verified_sha`, is part of `verdict_set_digest`, and `reverify` runs it again.
- `review` failure reasons: `review_findings` (with its failing findings in `rework_reason`), `review_output` (no usable block after two runs), `review_timeout` (the last run hit the reviewer's `timeout_secs`), and `review_unavailable` (the session's model was unavailable; see [A review wait on an unavailable model](#a-review-wait-on-an-unavailable-model)). `review_blocking` is the reason a verdict or task file written by an earlier build may carry; this build reads it and never writes it.
- When the profile lists both `commands` and `review`, the round's outcome follows the profile order.
  A `commands` failure decides it with the commands reason, even when the review passed or failed too: the review verdict is still written, its cost counts, `show` lists its items, and arc writes no automatic review rework for that round.
  When `commands` passes and `review` fails, the review path runs (see [A failed review](#a-failed-review)).
  Both verdicts are written when each stage ends, and a verdict already written is reused: a gate that died while the pair ran resumes only the stage without a verdict.
  The gate waits for both stages to end before it decides; a failing stage does not kill the other, and the review is bounded by its own limit.
  A stop signal to the gate reaches both running stages.

### A rework round

A review is a rework round when an earlier attempt of the same task has a review verdict for a `sha` other than the commit under review; the newest such verdict is the previous review.
A review of the same commit as the previous review (a `reverify` of an unchanged head) gets the first review's prompt: the full diff, and no finding is pre-existing.
It reads the reviewer's own settings, not its `first` table, because an earlier review verdict exists, so it is not a first review.
A genuine first review, one with no earlier verdict at all, keeps the full-diff prompt and the reviewer's `first` settings.

A rework round's prompt holds the brief, the previous review's failing findings (at or above that round's recorded `fix_level`, not `pre_existing`), the commit list `<previous sha>..<sha>`, and the increment instead of the full diff.
It lists each failing finding as `[severity] path:line summary — trigger`, with the ` — trigger` part left out when the trigger is empty, and it does not list a finding below that round's `fix_level` or a `pre_existing` one.
It asks the session to check that each failing finding is fixed, to review the increment, and to list every problem it finds; the session can read the files.

A previous verdict in the old shape has no `findings`: its `blocking` items are the failing findings the prompt lists and the window anchors below, at their own `level`, else `major`.

The increment is the task's own diff restricted to the hunks that hold a line the rework changed.
A line is an increment line when it is a new-side line of `git diff <previous sha> <sha>` and also a new-side line of the task's own diff, which starts at the merge base of the remote target branch and `sha`.
A line that came in only with a merge of main is not in the task's own diff, so it drops out.

arc, not the session, decides whether a finding is pre-existing.
After a rework round's report is parsed, a finding is marked `"pre_existing": true` unless one of these holds:

- its `file:line` is within 3 lines of a line the task's own change added or changed in that file (the new side of `git diff <the merge base of the remote target branch and the head>..<head>`): a problem in code the task itself wrote in an earlier attempt is the task's, not pre-existing, so the rework's increment does not decide this;
- it is in the same file as and within 10 lines of a failing finding of the previous review, so a finding that was not fixed stays failing; only the previous round's failing findings are anchors, never a finding below that round's `fix_level` or a `pre_existing` one;
- its `severity` is `critical`: a `critical` finding fails in every round, wherever it is;
- it has no `file` or no `line`: it stays failing at its severity, because the review prompt asks for both fields and an omission is not evidence that the problem is old.

A marked finding keeps its `severity` (`major` included), and a `major` finding in a file the task's own change does not touch, or more than 3 lines from any changed line in that file, is still marked, so the rework is not sent back for a problem that predates the task; `minor` findings follow the same window rules.
The stage's decision is the reviewer's `fix_level`: it passes when no finding at or above that level is left unmarked, and the verdict records how many findings arc marked as `review_pre_existing`.
The follow-up task of a plan carries a marked finding at its severity (see [The review's follow-up](#the-reviews-follow-up)).

When a rework round's review still fails on a failing finding and the plan's `auto` is not `off`, arc writes the next attempt's brief itself (see [A failed review](#a-failed-review)).

### A failed review

A review that failed with `review_findings` (or an old verdict's `review_blocking`) returns the task to `todo.ready` with a brief arc writes, the way a conflict with main does, when the plan's `auto` is not `off`.
The heading is `## Rework <n> (review)`, with the lead's `add_rework` numbering.
The text says the review found these items, to fix them and nothing else, to append commits and not to rebase; it lists the round's failing findings, one per line, as `[severity] path:line summary — trigger`, with the ` — trigger` part left out when the trigger is empty and the parts a finding does not have left out; a finding below `fix_level` or a `pre_existing` one is not listed; and it names the review verdict to read for detail.
The worker cannot read `runs/`, so the wrapper copies the verdict into the worker's worktree at `.radian/arc-verdicts/<task>/<attempt>/<file>` and puts that worktree-relative path in the brief; the task file keeps the absolute path. A verdict that is missing, is not a regular file, is larger than 1 MiB, or cannot be copied stays in the brief as it is, and the attempt record carries `verdict copy skipped: <path>: <reason>`, which [`radian arc show`](planning.md#radian-arc-show-id-json) prints as `attempt_note`.
It counts as an automatic rework: it raises `auto_rework`, and `max_auto_rework` pauses the plan (see the [`limits.max_auto_rework` row](config.md#configtoml)); the task then stays in `review.rework_pending`.
With `auto` `off`, the task stays in `review.rework_pending` and waits for you, as before.
`radian arc run` prints `<task> review blocking -> todo.ready (automatic rework)`.

### The review's follow-up

A task is **open** when it is in `todo.ready`, `doing.*` or `review.*`; a `todo.proposed`, `done.*` or `failed.*` task is not open.
When a plan has no open task, arc collects the review findings of the merged tasks that merged under this rule and proposes one follow-up task for the plan, and no more.
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

For each collected task arc reads every `review` verdict of the task, oldest attempt first, and collects every finding of every round that is below that round's `fix_level` or `pre_existing`; these are the findings the round did not fail on.
Every round is read, not only the newest: a round that failed can carry a below-threshold or pre-existing finding, and a later round can pass with nothing to carry.
Nothing from the failing findings is collected: a merged task's failing findings were fixed or the review would not have passed, so a round that only had failing findings contributes nothing.

Two findings of the same merged task are the same when their `file`, `line` and `summary` are equal after a leading tag that repeats the finding's own severity is removed and whitespace is collapsed; arc keeps the newest copy.
A finding with the same `file:line` and another summary is kept as its own finding.
Findings of different tasks are never merged.

When at least one item is left, and `[gate.review] propose_followups` is `true` (the default), arc adds one task to the plan, and no more:

- the id is the next task id;
- the slug is `<plan slug>-followups`, cut to the 48-character slug limit with a trailing `-` removed; when the plan already holds a task with that slug, `-2`, `-3`, … is appended within the limit;
- the title is `Follow-ups from <plan id>'s reviews`;
- its `after` is the collected merged tasks, in merge order;
- its phase is `todo.proposed`, so the loop never claims it until you confirm it (see [`confirm`](owner.md#radian-arc-confirm-id));
- its `allow_noop` is `true`: its brief lets each item be left with a one-line reason, so an attempt that adds no commit may deliver nothing (see [An empty change](gate.md#an-empty-change));

Its brief is fixed: one line saying the items are from the reviews of the plan's merged tasks and that each item is to be fixed or left with a one-line reason in the final answer; then one section per collected task, headed `From <task id>`, holding that task's findings grouped by the round they came from, each group headed `From review <n> (attempt <n>):`, one finding per line as `[severity] path:line summary — trigger`, with the ` — trigger` part left out when the trigger is empty; then the path of every review verdict.
The worker gets each verdict as a copy at `.radian/arc-verdicts/<task>/<attempt>/<file>` in its worktree, one copy for every task and attempt the brief names, and the task file keeps the absolute paths; the skip line is the same as for a review rework ([A failed review](#a-failed-review)).
A finding whose `summary` starts with a tag that repeats its own severity prints the severity once, as in the rework reason above.

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


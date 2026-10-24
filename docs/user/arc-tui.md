---
description: The arc TUI: the plans, task and merge pages, every mark and key, and the owner's actions.
---

# arc: the TUI

This page covers the TUI; the `radian arc` commands are in [`arc`](arc.md), the gate and its failures are in [`arc-gate`](arc-gate.md), and the files arc keeps are in [`arc-files`](arc-files.md).

`radian arc` opens a view of plans, tasks, verdicts, journals and merges; the pages themselves read the store, and the owner's keys run the same `radian-arc` library calls the CLI does (see [the owner's actions](#the-owners-actions)). It probes `runner.lock` (take and release at once, never creating it) and never holds it. Header: `arc`, tabs `plans · task · merge` (`merge` for a task in `review.*` or `done`), then on the plans page the view and the plans it hides (`active · 14 hidden`), `main <sha>` and the loop state.

## The loop

The TUI is never the scheduling loop: it holds no `runner.lock` and runs no pass. When it opens, it starts `radian arc run --watch` with the default interval when no loop holds the lock; `R` starts one the same way while none runs. The loop is a detached process in the repository root: it has its own session, its input and output go to `/dev/null`, and its lines go to [`loop.log`](arc-files.md). Starting a loop asks nothing: it is the default way arc runs.

The header reads the probe: `loop ● running (pid <n>)` while any loop holds the lock, whoever started it; `loop ◌ starting` from a start until a probe sees its lock; `loop ○ stopped` when none holds it. A start that fails (the process cannot start, or no lock appears within 10 seconds) shows `loop ○ stopped` and one footer line with the reason, the exit code when the process exited (`3` a rejected worker line, `4` another holder), and the last `radian arc: <message>` line of `loop.log` when there is one. `R` while a loop runs starts nothing and says so in the footer. Quitting the TUI never stops a loop: the loop keeps running, and the next time you open the TUI it shows that loop's pid.

## The ticker and the log

`loop.log` is the loop's own output, one line per printed line with an RFC 3339 UTC timestamp. The newest transition line the file gained since the TUI opened is the one-line ticker above the footer's key hints. `L` opens a view of the last 500 lines over the page and closes it again; `j`/`k` (or `↑`/`↓`) scroll it, `f` goes to its end, and `Esc` closes it. The view follows new lines while it is scrolled to the bottom. A repository with no `loop.log` shows `no loop log yet`.

## The plans page

Plan marks: the first rule in this table that holds draws the mark, in its theme role.

| Mark | Name | Rule | Theme role |
|---|---|---|---|
| `✕` | cancelled | the plan's `status` is `cancelled`; the rest of the row (the id and the summary) is drawn weak (`hint`) | `plan_cancelled` |
| `◌` | draft | `status` is `draft` | `plan_draft` |
| `◉` | needs you | a task is `review.awaiting_human` or `todo.proposed`; or no task is open (`todo.ready`, `todo.retry_wait`, `doing.*`, `review.*`) and a task is `failed.*` other than `failed.cancelled` | `plan_needs_you` |
| `●` | running | a task is `doing.*`, or in a `review.*` phase other than `awaiting_human` | `plan_running` |
| `◍` | paused | `auto` is `off` and a task is open | `plan_paused` |
| `○` | queued | a task is open, and nothing is running (`auto` is not `off`) | `plan_queued` |
| `✓` | done | every task is `done.*` or `failed.cancelled`, and at least one is `done` | `plan_done` |

A plan no rule matches (a confirmed plan with no tasks) draws `○`. After the mark come `P<n>` and the id, then a summary row of three aligned columns: the auto level, `<merged>/<total>` and the cost (`—` when none). Each column is padded to the widest value among the plans the view lists; the count and the cost are right-aligned, so `1/1` and `4/13`, and `$0.12` and `—`, end in one column. The right frame's label keeps the status word (`confirmed`, `draft`, `cancelled`), so it is still on screen for the selected plan. Frame: `<id> <status> · P<n> · auto <level> · <n>/<m> merged · <cost>`, then title, tasks: a task after a plan task sits under its first parent (`├─`/`└─`), other parents are `also after <ids>`, roots in plan order, each task once. A row: mark, id, `state.phase` (mark's color), `#attempt`, cost, and `waits <ids>` for an unmerged-parent todo. The `state.phase`, `#n`, cost and trailing text start at the same column on every row, flat or graph: the id block (`connector + mark + id`) has one width per plan, the `#n` column the widest attempt, and the cost column the widest cost, each cost right-aligned (`—` when none). A deep row's connector leaves its id less room, so the id is cut with `…`. A `doing` task's row also shows its worker's heartbeat: `heartbeat 12s ago`, in the warning color with `stalled?` once the beat is older than the attempt's stall window, and `starting` before the first beat; while nothing holds `runner.lock` the row adds `loop stopped`, because a stored state is not live progress. Marks: `✓` done.merged green, `×` failed red, `●` doing/verifying accent, `!` awaiting_human/rework_pending accent bold (you or the lead acts), `◻` todo weak. Keys: `↑`/`↓` move in the focused column (the plan list, or the task list after `→`), `→`/`←` focus the task list/plan list, `Tab`/`Enter` open the task page (`Esc` on it goes back, while `Tab`/`Shift-Tab` cycle its four tabs), `g` toggles the flat list (`after <ids>`), `f` cycles the plan view (below), `c` confirm a draft plan (plan column) or a `todo.proposed` task (task column), `x` cancel the selected item, `+`/`-` move the selected plan's priority one step toward `P0`/`P5`, `a` cycle the selected plan's auto level `off → run → merge → off` (only a confirmed plan), `p` pause it (`auto off`), `s` start the selected task, `R` start a loop when none runs (see [the loop](#the-loop)), `L` the log, `Esc`/`q`/`Ctrl-C` quit. A faint hint means the key cannot act now: `c` needs a draft plan in the focused plan column or a proposed task in the focused task column, `x` a selected item, `+`/`-` a plan that is not cancelled, `a` a confirmed one, `p` an auto level other than `off`, and `s` a `todo.ready` task in a confirmed plan whose `after` tasks are merged. Pressing `s` on a task that cannot start (or `c`/`a` on the wrong plan) puts the reason in the footer and starts nothing. A list longer than its area scrolls so the selected row stays visible: it moves only when the selection leaves the top or the bottom.

`x` always asks first, the way the merge page's `M` does: `cancel <task> ? y/n` for a task, or `cancel plan <plan> and its <N> open tasks? y/n` for a plan. With the task column focused it acts on the selected task; with the plan column focused it acts on the selected plan, and `N` counts the plan's tasks in `todo`, `doing` or `review` — the ones the plan form of [`cancel`](arc.md#radian-arc-cancel-id) ends. `y` runs the action and `n` or `Esc` changes nothing. `c` with the plan column focused confirms a draft plan; with the task column focused it confirms the selected `todo.proposed` task and leaves the plan's `status` and `auto` alone. When neither applies the hint is faint and the footer says `only a draft plan or a proposed task is confirmed`. `+` moves the selected plan's priority toward `P0`, `-` toward `P5`; at an end the footer says the plan is already there, and a cancelled plan gets the CLI's refusal. The list draws `P<n>` before the id and the frame label draws it after the status, each priority in its own `priority_p0`..`priority_p5` theme role (see [themes](themes.md#every-role)).

### The plan view

`f` cycles the plans page through four views, in this order: `active` → `all` → `done` → `cancelled` → `active`. The key works whichever column has the focus.

| View | Plans it lists |
|---|---|
| `active` (the default) | The plans `radian arc list` prints without `--all`: not cancelled, and not finished. A plan with no tasks counts as active. |
| `all` | Every plan. |
| `done` | The finished plans: every task `done.merged`, and not cancelled. |
| `cancelled` | The cancelled plans. |

A plan is finished by the same test `radian arc list` uses: every task is `done.merged`, so a plan whose tasks are all merged is finished even while its status is `draft`.

The header names the view and counts the plans it hides, for example `active · 14 hidden`; `all` hides nothing and shows no count. A view with no plan shows one weak line in the plan column (`no active plans`, `no plans`, `no done plans`, or `no cancelled plans`) and leaves the right column empty.

The selection stays on the same plan when that plan is in the new view; otherwise it moves to the first plan of the view. The task-column selection resets when the plan changes. The 2-second re-read keeps the view: a plan that leaves it (its last task merges in `active`, for example) disappears, and the selection follows the same rule. The view is not saved: every start opens on `active`.

## The task page

Task frame: `<id> <state.phase> · attempt <N> · <cost> · plan <id>`, the title, the four tabs `brief · conversation · diff · attempts` and `attempt 1 2 3` (latest by default; `[`/`]` switch). The page opens on `brief` while the task has no attempt (`attempt 0`) and on `conversation` once it has one; a tab you pick stays picked while the page stays on the same task. Brief: the task file's body, markdown through the same renderer the transcript uses, wrapped to the frame and inside its rail, rework sections (`## Rework <n>`) included, exactly as the store loads it. It is the task file, not the stdin a worker got: it draws no frontmatter (the frame label already names the id, state, attempt and plan), and the hand-off section a fresh attempt's stdin opens with (`## Where the previous attempt left off`) is not part of it. `j`/`k` scroll it and `f` follows its end, as on the conversation tab. Conversation: the selected attempt's worker journal through the attach view — its session id from `execution.json`, else `identity.json`, else the claim's `start.json` (the task id only when none of them names one), which names `~/.radian/sessions/<session id>.jsonl` (one weak line while the file does not exist yet; the 2-second refresh opens it when it appears, so a running attempt's conversation follows its worker); `j`/`k` scroll, `f` follows the growing journal. Diff: the merge commit's first-parent diff when merged, else the task's `base_sha..head_sha` (a merge that fast-forwarded main shows the whole branch, not only its last commit); a task in `doing.running` shows `live · worktree` instead: the worker's worktree against the attempt's `base_sha`, re-read every 5 seconds, uncommitted and untracked files included and marked `uncommitted`. Files show `+N`/`-N`, the selected file's diff is a frame titled with its path, `n`/`p` file, `j`/`k` scroll, `f` end. Attempts: each attempt's verdicts (`✓ commands`, `× integration (conflict)`). `Tab`/`Shift-Tab` (or `h`/`l`, `←`/`→`) change tab, `[`/`]` attempt, `m` open the merge page (only for a task in `review.*` or `done`), `L` the log, `Esc` back, `q`/`Ctrl-C` quit.

## The merge page

`m` on the task page opens the merge page (the header's `merge` tab names it), the task in an open frame titled `<task> verified <short sha> · attempt <N>`, with the title, then one row per stage of the selected profile plus the task's [`extra_stages`](arc-files.md#task-fields) in the fixed order `commands`, `mutation`, `review`, `ci`, `human`, `integration`. A row is the mark, the stage name, `on <short sha>` (the task's commit the stage verified), `→ candidate <sha> over main <sha>` when the stage ran on a merge of the task onto main, and `N commands · 5m54s` (`N sessions · …` for `review`, `approver auto` or `approver owner` for `human`). The `commands` and `integration` rows expand to one row per command (its own mark, the line, its seconds); `↑`/`↓` select a command row and `Enter` opens its log in a scrollable frame (`j`/`k`/`↑`/`↓` scroll it, `f` follows the end, `Esc` closes it). The log is read on a worker thread, from its end: the float shows the last 512 KiB (at most 5000 lines), so a log that is a whole test run never waits on a frame or loads whole into memory.

Marks: `✓` a verdict for the task's commit passed (green), `×` a verdict failed (red, with its reason), `●` the running gate's current stage (accent), `◻` weak — `not in this profile`, or `not run yet`. When `head_sha` no longer equals `verified_sha`, a verdict no longer counts: every mark turns `◻`, no row keeps its commands, duration, approver or candidate, and a row says `the branch moved: head <sha> is not verified <sha>; the task is back in verification`.

Then a `summary` frame: the `review` stage's `summary` (or a weak `no review stage in this profile`), then its non-empty groups — `blocking` (each item `×`, red), `suggestions` (`•`), `verified` (`✓`, green) — then the task's commits (`• <short sha> <subject>`, newest first). A moved branch shows a weak `no review for this commit: the branch moved` instead: the review was for the verified commit. Then the file list and the selected file's diff, exactly as the task page's diff tab (`n`/`p` file, `j`/`k` scroll, `f` end). `Esc` closes the log first, then goes back to the task page; `L` opens the loop's log; `q`/`Ctrl-C` quit, except that a `q` typed into the reject note is a letter of the note.

## The owner's actions

The TUI's actions are the CLI's own library calls, run in the TUI process; it never runs `radian arc` as a subprocess. While one runs the footer shows a spinner; its result line stays there until the next key — the CLI's own line (`TK0009-reverify done.merged (db2450cd)`), or the error text and exit code the CLI would print (`… (exit 3)`). An action that cannot take `state.lock` shows the CLI's message and changes nothing.

| Page | Key | Action | Confirmation |
|---|---|---|---|
| plans | `c` | `confirm` a draft plan (plan column) or a `todo.proposed` task (task column) | none |
| plans | `x` | `cancel` the selected task, or the selected plan and its open tasks | `cancel <task> ? y/n` / `cancel plan <plan> and its <N> open tasks? y/n` |
| plans | `+` / `-` | move the selected plan's priority one step toward `P0` / `P5` | none |
| plans | `a` | cycle the plan's auto level | a float for `merge` (`set auto merge on <plan>?`) |
| plans | `p` | `pause` (auto `off`) | none |
| plans | `s` | `start` the selected task | none |
| merge | `M` | `merge` the task | `merge <task> onto main? y/n` |
| merge | `r` | `reject` with a note | a one-line note float: `Enter` rejects, `Esc` cancels; an empty note is refused like the CLI |
| merge | `v` | `reverify` | none |

`y` confirms and `n` (or `Esc`) closes a confirmation float without changing anything. `start` in the TUI does what `radian arc start` does; the scheduling loop (the `radian arc run --watch` the TUI starts, or one started by hand) is what schedules the rest.

## Quitting

`q` (or `Ctrl-C`) quits, and so does SIGTERM: the terminal is restored and the TUI exits `0`. Nothing else happens: no float about the loop, no signal to a loop, whether the TUI started it or not. A loop the TUI started keeps running with its lock, and an attempt it left running is reconciled by the next loop, exactly as when `radian arc run --watch` is killed.

Re-reads the store every 2 s; git, the merge page's data, a command's log and the actions run in worker threads, a frame never waits (`loading…`).

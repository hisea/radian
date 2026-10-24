---
description: The arc TUI: the plans, board, task and merge pages, every mark and key, and the owner's actions.
---

# arc: the TUI

This page covers the TUI; the `radian arc` commands are in [`arc`](arc.md), the gate and its failures are in [`arc-gate`](arc-gate.md), and the files arc keeps are in [`arc-files`](arc-files.md).

`radian arc` opens a view of plans, tasks, verdicts, journals and merges; the pages themselves read the store, and the owner's keys run the same `radian-arc` library calls the CLI does (see [the owner's actions](#the-owners-actions)). It probes `runner.lock` (take and release at once, never creating it) and never holds it. Header: `arc`, tabs `plans · board · task · merge` (`merge` for a task in `review.*` or `done`), then on the plans page the view and the plans it hides (`active · 14 hidden`; a zero count is omitted), on the board page its scope (`all plans` or the plan id, and `finished` while the list is open), `main <sha>` and the loop state.

`b` opens the board from any page, the task page included; on the board it goes back to the plans page. Outside the task page `Tab` keeps its page meaning: on the plans page it opens the selected task's page, on the board the selected card's, and on the merge page it closes it, unless a command's log is open (`Esc` closes that first). On the task page `Tab`/`Shift-Tab` cycle that page's own four tabs instead; there `Esc` returns to the page the task page was opened from, and `m` opens the merge page.

## The loop

The TUI is never the scheduling loop: it holds no `runner.lock` and runs no pass. When it opens, it starts `radian arc run --watch` with the default interval when no loop holds the lock; `R` starts one the same way while none runs. The loop is a detached process in the repository root: it has its own session, its input and output go to `/dev/null`, and its lines go to [`loop.log`](arc-files.md). Starting a loop asks nothing: it is the default way arc runs.

The header reads the probe: `loop ● running (pid <n>)` while any loop holds the lock, whoever started it; the pid appears when the holder recorded a readable one, so a lock with none shows plain `loop ● running`. `loop ◌ starting` lasts from a start until the process the TUI started writes its own pid into `runner.lock`: a start in flight is followed through its child, not by a probe, so it does not see another process's lock until the child exits or the 10 seconds run out. `loop ○ stopped` is shown when none holds the lock. A start that fails (the process cannot start, it exits, or it records no pid within 10 seconds) shows `loop ○ stopped` and one footer line with the reason, the exit code when the process exited (`3` a rejected worker line, `4` another holder), and the last `radian arc: <message>` line of `loop.log` when there is one. `R` while a loop runs starts nothing and says so in the footer. Quitting the TUI never stops a loop: the loop keeps running, and the next time you open the TUI it shows that loop's pid.

## The ticker and the log

`loop.log` is the loop's own output, one line per printed line with an RFC 3339 UTC timestamp. The newest transition line the file gained since the TUI opened is the one-line ticker above the footer's key hints. `L` opens a view of the last 500 lines over the page and closes it again; `j`/`k` (or `↑`/`↓`) scroll it, `f` goes to its end, and `Esc` closes it. The view follows new lines while it is scrolled to the bottom. A repository with no `loop.log` shows `no loop log yet`.

## The plans page

Plan marks: the first rule in this table that holds draws the mark, in its theme role.

| Mark | Name | Rule | Theme role |
|---|---|---|---|
| `✕` | cancelled | the plan's `status` is `cancelled`, or it has at least one task and every task is `failed.cancelled`; the rest of the row (the id and the summary) is drawn weak (`hint`) | `plan_cancelled` |
| `◌` | draft | `status` is `draft` | `plan_draft` |
| `◉` | needs you | a task is `review.awaiting_human` or `todo.proposed`; or no task is open (`todo.ready`, `todo.retry_wait`, `doing.*`, `review.*`) and a task is `failed.*` other than `failed.cancelled` | `plan_needs_you` |
| `●` | running | a task is `doing.*`, or in a `review.*` phase other than `awaiting_human` | `plan_running` |
| `◍` | paused | `auto` is `off` and a task is open | `plan_paused` |
| `○` | queued | a task is open, and nothing is running (`auto` is not `off`) | `plan_queued` |
| `✓` | done | every task is `done.merged` or `failed.cancelled`, and at least one is `done.merged` (the same finished rule `radian arc list` uses) | `plan_done` |

A plan no rule matches (a confirmed plan with no tasks) draws `○`. After the mark come `P<n>` and the id, then a summary row of three aligned columns: the auto level, `<merged>/<total>` and the cost (`—` when none). `total` counts the plan's tasks that are not `failed.cancelled`, so a plan with one merged task and one cancelled follow-up reads `1/1` and a plan whose tasks are all cancelled reads `0/0`; the frame label's `<n>/<m> merged` uses the same counts. Each column is padded to the widest value among the plans the view lists; the count and the cost are right-aligned, so `1/1` and `4/13`, and `$0.12` and `—`, end in one column. The right frame's label keeps the status word (`confirmed`, `draft`, `cancelled`), so it is still on screen for the selected plan. Frame: `<id> <status> · P<n> · auto <level> · <n>/<m> merged · <cost>`, then title, tasks: a task after a plan task sits under its first parent (`├─`/`└─`), other parents are `also after <ids>`, roots in plan order, each task once. A row: mark, id, `state.phase` (mark's color), `#attempt`, cost, and `waits <ids>` for an unmerged-parent todo. The `state.phase`, `#n`, cost and trailing text start at the same column on every row, flat or graph: the id block (`connector + mark + id`) has one width per plan, the `#n` column the widest attempt, and the cost column the widest cost, each cost right-aligned (`—` when none). A deep row's connector leaves its id less room, so the id is cut with `…`. A `doing` task's row also shows its worker's heartbeat: `heartbeat 12s ago`, in the warning color with `stalled?` once the beat is older than the attempt's stall window, `starting` before the first beat, and `heartbeat unreadable` in the warning color when the beat file exists but its time cannot be used (it does not parse, or it is in the future); while nothing holds `runner.lock` the row adds `loop stopped`, because a stored state is not live progress. Marks: `✓` done.merged green, `×` failed red (a `failed.cancelled` task included: it stays listed under its plan), `●` doing/verifying accent, `!` awaiting_human/rework_pending accent bold (you or the lead acts), `◻` todo weak. Keys: `↑`/`↓` move in the focused column (the plan list, or the task list after `→`), `→`/`←` focus the task list/plan list, `Tab`/`Enter` open the task page (`Esc` on it goes back, while `Tab`/`Shift-Tab` cycle its four tabs), `g` toggles the flat list (`after <ids>`), `f` cycles the plan view (below), `o` cycles the list's order (below), `c` confirm a draft plan (plan column) or a `todo.proposed` task (task column), `x` cancel the selected item, `+`/`-` move the selected plan's priority one step toward `P0`/`P5`, `a` cycle the selected plan's auto level `off → run → merge → off` (only a confirmed plan), `p` pause it (`auto off`), `s` start the selected task, `R` start a loop when none runs (see [the loop](#the-loop)), `L` the log, `Esc`/`q`/`Ctrl-C` quit. A faint hint means the key cannot act now: `c` needs a draft plan in the focused plan column or a proposed task in the focused task column, `x` a selected item, `+`/`-` a plan that is not cancelled, `a` a confirmed one, `p` an auto level other than `off`, `R` a loop that is neither running nor starting, and `s` a `todo.ready` task in a confirmed plan whose `after` tasks are merged. Pressing `s` on a task that cannot start (or `c`/`a` on the wrong plan) puts the reason in the footer and starts nothing. A list longer than its area scrolls so the selected row stays visible: it moves only when the selection leaves the top or the bottom.

`x` always asks first, the way the merge page's `M` does: `cancel <task> ? y/n` for a task, or `cancel plan <plan> and its <N> open tasks? y/n` for a plan. With the task column focused it acts on the selected task; with the plan column focused it acts on the selected plan, and `N` counts the plan's tasks in `todo`, `doing` or `review` — the ones the plan form of [`cancel`](arc.md#radian-arc-cancel-id) ends. `y` runs the action and `n` or `Esc` changes nothing. `c` with the plan column focused confirms a draft plan; with the task column focused it confirms the selected `todo.proposed` task and leaves the plan's `status` and `auto` alone. When neither applies the hint is faint and the footer says `only a draft plan or a proposed task is confirmed`. `+` moves the selected plan's priority toward `P0`, `-` toward `P5`; at an end the footer says the plan is already there, and a cancelled plan gets the CLI's refusal. The list draws `P<n>` before the id and the frame label draws it after the status, each priority in its own `priority_p0`..`priority_p5` theme role (see [themes](themes.md#every-role)).

### The plan view

`f` cycles the plans page through four views, in this order: `active` → `all` → `done` → `cancelled` → `active`. The key works whichever column has the focus.

| View | Plans it lists |
|---|---|
| `active` (the default) | The plans `radian arc list` prints without `--all`: not cancelled, and not finished. A plan with no tasks counts as active. |
| `all` | Every plan. |
| `done` | The finished plans: at least one task, every task `done.merged` or `failed.cancelled`, at least one `done.merged`; not cancelled. |
| `cancelled` | The cancelled plans: `status` `cancelled`, or every task `failed.cancelled`. |

A plan is finished by the same test `radian arc list` uses: it has at least one task, every task is `done.merged` or `failed.cancelled`, and at least one task is `done.merged`, so a plan whose tasks are all merged is finished even while its `status` is `draft`. A plan with no tasks is never finished. A plan whose tasks are all `failed.cancelled` is cancelled by its tasks: it draws `✕`, is in `cancelled` and not in `active` or `done`, and its file's `status` is not rewritten. Both tests live in `radian-arc`, so the views and the `✓` and `✕` marks cannot disagree with `radian arc list`.

The header names the view and counts the plans it hides, for example `active · 14 hidden`; `all` shows no count, and a count of `0` is left out (as `radian arc list` leaves out a zero part), so a `done` view that hides no plan shows just `done`. A view with no plan shows one weak line in the plan column (`no active plans`, `no plans`, `no done plans`, or `no cancelled plans`) and leaves the right column empty.

The selection stays on the same plan when that plan is in the new view; otherwise it moves to the first plan of the view. The task-column selection resets when the plan changes. The 2-second re-read keeps the view: a plan that leaves it (its last task merges in `active`, for example) disappears, and the selection follows the same rule. The view is not saved: every start opens on `active`.

### The list's order

`o` cycles the list's order: `priority` → `state` → `id` → `priority`. The order is not saved: every start opens on `priority`. The `PLANS` heading names the current order (`PLANS · by priority`, `PLANS · by state`, `PLANS · by id`).

| Order | The list's order |
|---|---|
| `priority` (the default) | `P0` first, then `P1` … `P5`; within one priority, the id ascending. A plan without a `priority` field ranks as `P3`. |
| `state` | By the plan's mark (above): needs you `◉`, running `●`, queued `○`, paused `◍`, draft `◌`, done `✓`, cancelled `✕`; within one state, by priority and then id. |
| `id` | The id ascending. |

The order applies to the plans page's left list only; `radian arc list` keeps its claim order. The selection stays on the same plan when the order changes, when a refresh reorders the list, and with `f`; the task column follows the selected plan.

## The board page

The board groups the open tasks of the plans it shows by who acts next. From left to right:

| Column | Tasks |
|---|---|
| `todo` | `todo.ready`, `todo.blocked` and `todo.retry_wait`: waiting for its turn. |
| `doing` | `doing.starting`, `doing.running`, `doing.reconciling`: a worker runs. |
| `verifying` | `review.verifying`, `review.integrating`, `review.ready_to_merge`: a gate or a merge runs. Also `review.awaiting_human` of a plan whose `auto` is `merge` (the loop merges it), and `review.rework_pending` that a scheduling pass will still reverify by itself (a `commands` or integration command failure or timeout, with automatic reverifies left and the `(head, main)` pair not re-verified yet). |
| `needs you` | `review.awaiting_human` of a plan whose `auto` is not `merge`; `review.rework_pending` that nothing automatic will leave (the review or conflict limit was reached, a command failure whose `(head, main)` pair was re-verified already or whose count is used up, or a reject note); `todo.proposed`; `todo.paused_budget`; and every task of a plan `arc` paused through one of its tasks: the automatic rework limit wrote `paused.json`, or a plan budget set the plan's `auto` to `off` and left a task in `todo.paused_budget`. A task budget alone does not pause the plan. |

Within a column, cards keep the order `radian arc list` prints: the plan's priority (`P0` first), the plan id, then the task's order in the plan. Each column is an open frame: its top line carries the column name and its count (`╭─ TODO 10 ───…`), every row starts with the `│` rail, and the column ends with `╰──`. Two cards inside a frame are separated by one weak `┄` divider, and no divider follows the last card. A card is three lines: the state mark, the task id (cut with `…`) and `#<attempt>` right-aligned; the title, indented under the id; and, weak, `<plan id short> · <phase>` on the left — `PL0012` is the plan id's prefix and number — with the cost (`$0.13`, or `—` when none) on the right, or for a `doing` card the age of the worker's last heartbeat (`5s`, `2m`). A card in `needs you` carries `!` instead of its state mark. The selection cursor `❯` sits in the marker column left of the rail, on the selected card's first line; the selected card is not otherwise marked.

The board shows the tasks of the plans the plans page's current view shows (the [`f` view](#the-plan-view), `active` by default). `p` limits it to the plan of the selected card and the header names it (`board · PL0012`); `p` again shows every plan of the view again (`board · all plans`).

### Widths and keys

A column reads at 28 cells. `h`/`l` (or `←`/`→`) change column, `j`/`k` (or `↑`/`↓`) move within a column, and each column scrolls to keep the selected card visible. `Enter` (or `Tab`) opens the selected card's task page; `Esc` returns to the plans page, and the task page returns to the board.

- **Wide** (at least 118 cells: four 28-cell columns and their two-cell gaps): the four columns side by side, each one open frame.
- **Medium** (at least 58 cells): two columns. The left stacks `todo` over `doing`, the right `verifying` over `needs you`; each group is its own frame with its name and count. `h`/`l` move between the left and the right column (`todo` ↔ `verifying`, `doing` ↔ `needs you`); `j`/`k` walk the cards of both groups of that column.
- **Narrow** (fewer than 58 cells): one column with the four groups' frames stacked in the order `todo`, `doing`, `verifying`, `needs you`, each with its name and count. `j`/`k` walk every card in that order; `h`/`l` jump to the previous or next group.

A group with no card shows its name, `0` and one weak line inside its frame: `nothing waiting`, `nothing running`, `nothing verifying`, or `nothing needs you`. The board re-reads the store every 2 seconds. The selection stays on the same task while it is on the board (also while the width changes), and moves to the nearest card of its column when the task leaves.

### The finished list

`d` replaces the columns with one open frame titled `FINISHED <n>`, where `n` is the number of tasks, holding every `done.merged` and `failed.*` task of the plans the board shows, the most recently finished first (the time is from the task's records: the passing integration verdict of a merged task, else the attempt's end record). One row each, starting with the frame's rail: `✓` (green) for a merged task or `×` (red) for a failed one, the task id, the title, the plan, and when (`12m ago`, `3h ago`, `2d ago`). The selection cursor `❯` sits left of the rail, and no dividers separate the rows. `j`/`k` scroll it; `Enter` opens the task page; `d` or `Esc` returns to the columns. An empty list shows one weak `nothing finished` inside the frame.

### The owner's actions on the board

The plans page's owner keys work on the selected card, with the same rules, floats and footer lines: `s` starts a `todo.ready` task (a refusal names why), `c` confirms a `todo.proposed` task, `x` cancels the task after `cancel <task> ? y/n`, `a` cycles the card's plan's auto level (setting `merge` asks), and `+`/`-` move the plan's priority. `pause` is not on the board: `p` is the plan scope. `M`, `r` and `v` (and `m`) open the merge page of a card in `review.*`, whose own keys then merge, reject or reverify it; `Esc` there returns to the board. `q`/`Ctrl-C` quit.

## The task page

Task frame: `<id> <state.phase> · attempt <N> · <cost> · plan <id>`, the title, the four tabs `brief · conversation · diff · attempts` and `attempt 1 2 3` (latest by default; `[`/`]` switch). The page opens on `brief` while the task has no attempt (`attempt 0`) and on `conversation` once it has one; a tab you pick stays picked while the page stays on the same task. Brief: the task file's body, markdown through the same renderer the transcript uses, wrapped to the frame and inside its rail, rework sections (`## Rework <n>`) included, exactly as the store loads it. It is the task file, not the stdin a worker got: it draws no frontmatter (the frame label already names the id, state, attempt and plan), and the hand-off section a fresh attempt's stdin opens with (`## Where the previous attempt left off`) is not part of it. `j`/`k` scroll it and `f` follows its end, as on the conversation tab. Conversation: the selected attempt's worker journal through the attach view — its session id from `execution.json`, else `identity.json`, else the claim's `start.json` (the task id only when none of them names one), which names `~/.radian/sessions/<session id>.jsonl` (one weak line while the file does not exist yet; the 2-second refresh opens it when it appears, so a running attempt's conversation follows its worker); `j`/`k` scroll, `f` follows the growing journal. Diff: the task's own change, from the merge base of the remote main and the selected attempt's head to that head — the range the review stage reads, so a branch that merged a newer main does not list main's changes; a merged task shows its merge commit's first-parent diff instead (a merge that fast-forwarded main shows the whole branch, not only its last commit); when the merge base cannot be taken the range falls back to the attempt's `base_sha..head_sha` and the weak line says why: `base..head · no remote main` when there is no remote main ref, `base..head · no merge base with main` when that ref exists but shares no ancestor with the head. A task in `doing.running` shows `live · worktree` instead: the worker's worktree against the merge base of the remote main and the worktree's HEAD (the attempt's base when the merge base cannot be taken, then `live · worktree · no remote main` or `live · worktree · no merge base with main`), re-read every 5 seconds, uncommitted and untracked files included and marked `uncommitted`. Files show `+N`/`-N`; the list takes at most half of the diff area's rows (at least 3 while there are that many files) and the selected file's frame takes the rest; it scrolls so the selected file stays visible, and when it does not show every file its last row says which are shown (`files 13–32 of 40`, weak). The selected file's diff is a frame titled with its path, `n`/`p` file, `j`/`k` scroll, `f` end. Attempts: each attempt's verdicts (`✓ commands`, `× integration (conflict)`). `Tab`/`Shift-Tab` (or `h`/`l`, `←`/`→`) change tab, `[`/`]` attempt, `m` open the merge page (only for a task in `review.*` or `done`), `L` the log, `Esc` back, `q`/`Ctrl-C` quit.

## The merge page

`m` on the task page opens the merge page (the header's `merge` tab names it), the task in an open frame titled `<task> verified <short sha> · attempt <N>`, with the title, then one row per stage of the selected profile plus the task's [`extra_stages`](arc-files.md#task-fields) in the fixed order `commands`, `mutation`, `review`, `ci`, `human`, `integration`. A row is the mark, the stage name, `on <short sha>` (the task's commit the stage verified), `→ candidate <sha> over main <sha>` when the stage ran on a merge of the task onto main, and `N commands · 5m54s` (`N sessions · …` for `review`, `approver auto` or `approver owner` for `human`). The `commands` and `integration` rows expand to one row per command (its own mark, the line, its seconds); `↑`/`↓` select a command row and `Enter` opens its log in a scrollable frame (`j`/`k`/`↑`/`↓` scroll it, `f` follows the end, `Esc` closes it). The log is read on a worker thread, from its end: the float shows the last 512 KiB (at most 5000 lines), so a log that is a whole test run never waits on a frame or loads whole into memory.

Marks: `✓` a verdict for the task's commit passed (green), `×` a verdict failed (red, with its reason), `●` the running gate's current stage (accent), `◻` weak — `not in this profile`, or `not run yet`. When `head_sha` no longer equals `verified_sha`, a verdict no longer counts: every mark turns `◻`, no row keeps its commands, duration, approver or candidate, and a row says `the branch moved: head <sha> is not verified <sha>; the task is back in verification`.

Then a `summary` frame: the `review` stage's `summary` (or a weak `no review stage in this profile`), then its non-empty groups — `blocking` (each item `×`, red), `suggestions` (`•`), `verified` (`✓`, green) — then the task's commits (`• <short sha> <subject>`, newest first). A moved branch shows a weak `no review for this commit: the branch moved` instead: the review was for the verified commit. Then the file list and the selected file's diff, exactly as the task page's diff tab (`n`/`p` file, `j`/`k` scroll, `f` end). `Esc` closes the log first, then goes back to the task page; `L` closes an open command log and opens the loop's log; `q`/`Ctrl-C` quit, except that a `q` typed into the reject note is a letter of the note.

## The owner's actions

The TUI's owner actions are the CLI's own library calls, run in the TUI process; unlike the loop it starts (see [the loop](#the-loop)), an action never runs `radian arc` as a subprocess. While one runs the footer shows a spinner; its result line stays there until the next key — the CLI's own line (`TK0009-reverify done.merged (db2450cd)`), or the error text and exit code the CLI would print (`… (exit 3)`). An action that cannot take `state.lock` shows the CLI's message and changes nothing.

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

---
description: The arc TUI: the plans, task and merge pages, every mark and key, and the owner's actions.
---

# arc: the TUI

This page covers the TUI; the `radian arc` commands are in [`arc`](arc.md), the gate and its failures are in [`arc-gate`](arc-gate.md), and the files arc keeps are in [`arc-files`](arc-files.md).

`radian arc` opens a view of plans, tasks, verdicts, journals and merges; the pages themselves read the store, and the owner's keys run the same `radian-arc` library calls the CLI does (see [the owner's actions](#the-owners-actions)). It takes `runner.lock` without waiting when it opens: with it, this window is the scheduling loop (see [the loop](#the-loop)); without it, it is a viewer with the owner's actions and takes the lock on a later refresh. Header: `arc`, tabs `plans · task · merge` (`merge` for a task in `review.*` or `done`), `main <sha>` and the loop state: `loop ● running (this window)`, `loop ● running (pid <n>)` for another holding process, or `loop ◻ stopped`.

## The loop

While the TUI is open it is the scheduling loop when it can be (ADR 0048 decision 13). With `runner.lock` it runs the same pass `radian arc run --watch` runs, on a background thread: 30 seconds after a pass that ends quiet, and as soon as one of your actions changed state. The pass makes the auto levels' transitions (it starts a task at `auto` `run` or `merge`, runs the gate, and merges at `merge`); it does not wait for a worker or a gate, and the frame never waits for it. The header shows `loop ● running (this window)`.

When another process holds `runner.lock` (another `radian arc run`, a `run --watch`, or another TUI) this TUI is a viewer: it schedules nothing, your actions still run, and the header shows `loop ● running (pid <n>)` with the pid the holder wrote in the lock. Each time the view re-reads the store (every 2 seconds) it tries the lock again; when the other loop exits, the TUI takes it and becomes the loop. When the TUI closes, nothing advances.

## The ticker and the log

The pass's transitions appear as they happen: each transition line, the same text `radian arc run` prints, becomes the one-line ticker above the footer's key hints and is appended to the log. `L` opens the log over the page and closes it again; `j`/`k` (or `↑`/`↓`) scroll it, `f` goes to its end, and `Esc` closes it. The log also holds each pass's summary and one `radian arc: <message>` line for a pass that returned an error; the loop goes on with the next pass.

## The plans page

Plan marks: `✓` (merged, green), `●` (confirmed, accent), `◻` (draft, weak), `×` (cancelled, red), then id and `auto <level> · <merged>/<total> · <cost>` (`draft · ` or `cancelled · ` first; `—` when no cost). Frame: `<id> <status> · auto <level> · <n>/<m> merged · <cost>`, then title, tasks: a task after a plan task sits under its first parent (`├─`/`└─`), other parents are `also after <ids>`, roots in plan order, each task once. A row: mark, id, `state.phase` (mark's color), `#attempt`, cost, and `waits <ids>` for an unmerged-parent todo. Marks: `✓` done.merged green, `×` failed red, `●` doing/verifying accent, `!` awaiting_human/rework_pending accent bold (you or the lead acts), `◻` todo weak. Keys: `↑`/`↓` move in the focused column (the plan list, or the task list after `→`), `→`/`←` focus the task list/plan list, `Tab`/`Enter` open the task page (`Tab` on it goes back), `g` toggles the flat list (`after <ids>`), `c` confirm a draft plan, `a` cycle the selected plan's auto level `off → run → merge → off` (only a confirmed plan), `p` pause it (`auto off`), `s` start the selected task, `L` the log, `Esc`/`q`/`Ctrl-C` quit. A faint hint means the key cannot act now: `c` needs a draft plan, `a` a confirmed one, `p` an auto level other than `off`, and `s` a `todo.ready` task in a confirmed plan whose `after` tasks are merged. Pressing `s` on a task that cannot start (or `c`/`a` on the wrong plan) puts the reason in the footer and starts nothing. A list longer than its area scrolls so the selected row stays visible: it moves only when the selection leaves the top or the bottom.

## The task page

Task frame: `<id> <state.phase> · attempt <N> · <cost> · plan <id>`, the title, `conversation · diff · attempts` and `attempt 1 2 3` (latest by default; `[`/`]` switch). Conversation: the selected attempt's worker journal through the attach view — the session id in its `execution.json`, or the task id when it wrote none (see [worker sessions](arc-gate.md#failures-retries-and-the-budget)), names `~/.radian/sessions/<session id>.jsonl` (one weak line if none); `j`/`k` scroll, `f` follows the growing journal. Diff: the merge commit's first-parent diff when merged, else the task's `base_sha..head_sha` (a merge that fast-forwarded main shows the whole branch, not only its last commit); a task in `doing.running` shows `live · worktree` instead: the worker's worktree against the attempt's `base_sha`, re-read every 5 seconds, uncommitted and untracked files included and marked `uncommitted`. Files show `+N`/`-N`, the selected file's diff is a frame titled with its path, `n`/`p` file, `j`/`k` scroll, `f` end. Attempts: each attempt's verdicts (`✓ commands`, `× integration (conflict)`). `h`/`l` (or `←`/`→`) change tab, `[`/`]` attempt, `m` open the merge page (only for a task in `review.*` or `done`), `L` the log, `Tab`/`Esc` back, `q`/`Ctrl-C` quit.

## The merge page

`m` on the task page opens the merge page (the header's `merge` tab names it), the task in an open frame titled `<task> verified <short sha> · attempt <N>`, with the title, then one row per stage of the selected profile plus the task's [`extra_stages`](arc-files.md#task-fields) in the fixed order `commands`, `mutation`, `review`, `ci`, `human`, `integration`. A row is the mark, the stage name, `on <short sha>` (the task's commit the stage verified), `→ candidate <sha> over main <sha>` when the stage ran on a merge of the task onto main, and `N commands · 5m54s` (`N sessions · …` for `review`, `approver auto` or `approver owner` for `human`). The `commands` and `integration` rows expand to one row per command (its own mark, the line, its seconds); `↑`/`↓` select a command row and `Enter` opens its log in a scrollable frame (`j`/`k`/`↑`/`↓` scroll it, `f` follows the end, `Esc` closes it). The log is read on a worker thread, from its end: the float shows the last 512 KiB (at most 5000 lines), so a log that is a whole test run never waits on a frame or loads whole into memory.

Marks: `✓` a verdict for the task's commit passed (green), `×` a verdict failed (red, with its reason), `●` the running gate's current stage (accent), `◻` weak — `not in this profile`, or `not run yet`. When `head_sha` no longer equals `verified_sha`, a verdict no longer counts: every mark turns `◻`, no row keeps its commands, duration, approver or candidate, and a row says `the branch moved: head <sha> is not verified <sha>; the task is back in verification`.

Then a `summary` frame: the `review` stage's `summary` (or a weak `no review stage in this profile`), then its non-empty groups — `blocking` (each item `×`, red), `suggestions` (`•`), `verified` (`✓`, green) — then the task's commits (`• <short sha> <subject>`, newest first). A moved branch shows a weak `no review for this commit: the branch moved` instead: the review was for the verified commit. Then the file list and the selected file's diff, exactly as the task page's diff tab (`n`/`p` file, `j`/`k` scroll, `f` end). `Esc` closes the log first, then goes back to the task page; `L` opens the loop's log; `q`/`Ctrl-C` quit, except that a `q` typed into the reject note is a letter of the note.

## The owner's actions

The TUI's actions are the CLI's own library calls, run in the TUI process; it never runs `radian arc` as a subprocess. While one runs the footer shows a spinner; its result line stays there until the next key — the CLI's own line (`TK0009-reverify done.merged (db2450cd)`), or the error text and exit code the CLI would print (`… (exit 3)`). An action that cannot take `state.lock` shows the CLI's message and changes nothing.

| Page | Key | Action | Confirmation |
|---|---|---|---|
| plans | `c` | `confirm` a draft plan | none |
| plans | `a` | cycle the plan's auto level | a float for `merge` (`set auto merge on <plan>?`) |
| plans | `p` | `pause` (auto `off`) | none |
| plans | `s` | `start` the selected task | none |
| merge | `M` | `merge` the task | `merge <task> onto main? y/n` |
| merge | `r` | `reject` with a note | a one-line note float: `Enter` rejects, `Esc` cancels; an empty note is refused like the CLI |
| merge | `v` | `reverify` | none |

`y` confirms and `n` (or `Esc`) closes a confirmation float without changing anything. `start` in the TUI does what `radian arc start` does; the TUI's own loop is what schedules the rest while it holds `runner.lock` (see [the loop](#the-loop)).

## Quitting

`q` (or `Ctrl-C`) quits. While this window holds `runner.lock` and an attempt is running, `q` asks first: the loop stops, the running attempts keep running, and the next loop reconciles them; `y` quits and `n` (or `Esc`) stays. With no attempt running, `q` quits at once.

Quitting and SIGTERM do what `radian arc run --watch` does on SIGTERM: no new worker and no new gate start, a running gate gets `[limits] cancel_grace_secs` and is then killed, the pass's current step is given a short grace rather than waited out, the pass is otherwise left to the next loop's reconcile exactly as when `arc run --watch` is killed, `runner.lock` is released, and the TUI exits `0`.

Re-reads the store every 2 s; git, the merge page's data, a command's log and the actions run in worker threads, a frame never waits (`loading…`).

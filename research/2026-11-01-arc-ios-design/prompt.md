Design the iPhone app for **arc**, the plan-and-task orchestrator of the radian coding agent. The app is a thin client: it talks to one "hub" (a loopback HTTP API on the user's computer, reached over Tailscale), shows the state of every registered repository, and lets the owner approve, merge, reject or cancel work. It never runs agents and never shows live chat.

## Who uses it and when

One person, the owner of the repositories, usually away from the desk: phone in hand, 20 seconds at a time. Typical moment: a lock-screen notification says "radian · TK0283 awaiting your review"; they tap it, land on that task, read the gate results and the diff summary, and tap Merge or Reject with a note. The other moment: a glance at "what needs me" across all repositories.

## Data model (fixed, do not invent fields)

- **Repository**: name, path, whether the hub schedules it, lock holder.
- **Plan**: id like `PL0078-memory-model-written-files`, title, status `draft | confirmed`, `auto` level `off | run | merge`, `paused`, priority `P0..P3`, cost so far, task progress (merged / total).
- **Task**: id like `TK0271-memory-index-injection`, title, `state.phase` from this closed set:
  `todo.ready | todo.blocked | todo.retry_wait | todo.paused_budget | todo.proposed`,
  `doing.starting | doing.running | doing.reconciling`,
  `review.verifying | review.rework_pending | review.awaiting_human | review.integrating | review.ready_to_merge`,
  `done.merged`, `failed.exhausted | failed.cancelled | failed.irrecoverable`;
  plus `after` (task ids it waits on), attempt number, cost, `verified_sha`, rework reason.
- **Attempt**: number, start/end time, worker model, cost, wall time, queue wait.
- **Verdict** (one per gate stage per attempt): stage `commands | mutation | review | ci | human | integration`, result pass/fail, reason, time. A **review** verdict carries findings `{severity: critical | major | minor, file, line, summary, trigger, pre_existing}`.
- **Report**: the worker's final message (markdown text).
- **Diff**: file list with +added/−removed per file; per-file unified diff on demand, truncated past 256 KiB.

## Owner actions (the only writes)

`confirm` (a draft plan, or a `todo.proposed` task), `merge`, `reject` (a note is required, never empty), `cancel`, `auto` (set off/run/merge), `pause`, `reverify`, `priority`. Four of them need a second confirmation in the app before the request is sent: **merge**, **reject**, **cancel**, and **setting auto to `merge`**. The API returns the CLI's sentence on refusal (for example "the task is not in a state that allows merge" or a held lock): show it verbatim, never rephrase.

## Screens to design

1. **Needs you** (home): one list across all repositories, ordered like the terminal UI's "needs-you" rows: tasks in `review.awaiting_human` first, then `todo.proposed` and draft plans awaiting confirm, then `review.rework_pending`, then paused plans; everything else below, grouped by repository. Each row: repository, id, title, state.phase, cost, age. Pull to refresh re-reads the lists (events are hints only, never replayed).
2. **Plan**: request text (markdown, read-only; a confirmed plan only gains appended change records), the task list with dependencies (`after`) visible, progress, `auto` level control, pause, cost against budget.
3. **Task**: header with state.phase and attempt; sections for gate verdicts per stage (each verdict names the sha it judged; a moved branch invalidates them), the report, the diff (file list, tap a file for its diff), review findings grouped by severity with pre-existing ones marked, and the attempt switcher (newest first). Actions in a bottom bar, only the ones the state allows.
4. **Merge sheet**: the summary the owner reads before merging: verified sha, every required stage green or not, integration result against the current main, diff size; one Merge button with the confirmation step; the Reject path with a required note field.
5. **Repositories**: the hub's repository list with scheduled/lock status; a repository's plans.
6. **Pairing**: scan the QR the hub prints (`radian://hub?url=…&token=…`), store the token in the Keychain, show the hub address; a "rotate token on the computer" hint when the hub answers 401.
7. **Notification landing**: opened from a Bark/ntfy notification deep link `radian://hub/repos/<name>/items/<task>`, straight to that task.

Also design the states that will actually happen: hub unreachable (off the tailnet), 401 after a token rotation, an empty "nothing needs you" home, a plan paused by `max_auto_rework`, a stale list after reconnect (show "refreshing…" then the re-read).

## Visual language (match the existing product, do not restyle it)

radian's look is a drafting sheet: hairline rules, generous whitespace, mono labels, one saturated violet accent (`#5b3fd0` light / `#a793ff` dark), no cards with shadows, no gradients, no illustration. Typography: Geist for text, JetBrains Mono for ids, shas, paths, diffs and state.phase. The terminal UI it mirrors uses a single 2-cell marker column and thin vertical rules instead of boxes; carry that feel into iOS with native SwiftUI components (List, NavigationStack, sheets, SF Symbols) rather than a custom chrome. States get one glyph each and a role color: success green, error red, weak gray for not-yet, accent violet for "needs you". Dark and light, both. English UI copy, terse, no exclamation marks, no marketing tone.

## Constraints

Native iOS 17+ SwiftUI, iPhone only for v1. No live session view, no push registration (notifications come from the owner's own hook via Bark or ntfy), no editing of briefs or plans, no starting tasks. Everything the app can do, the CLI can do; the app adds nothing the API lacks.

## Deliverables

Screens 1–7 at iPhone 15 Pro size in light and dark, the four confirmation dialogs, the error/empty states listed above, a one-page component sheet (state glyphs with their colors, the verdict row, the finding row, the diff row), and a short rationale for each screen's hierarchy. Use the ids and titles above as sample data; invent no new fields or actions.

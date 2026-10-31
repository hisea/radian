---
id: TK0236
slug: stopfailure-and-notification
base: fb9a5ae6a895c3612f02720481eb64d96f44aedf
outcome: fail
crates: radian-host, radian-exec, radian-runtime, radian-permissions, radian-tui, radian-coding
---
## Brief

## Problem
When a turn ends on a model error, an unattended `radian exec` or arc worker tells no one. Without herdr, a TUI user gets no signal when radian waits for approval or for input.

## Decisions
Read `docs/backlog/hooks-stopfailure-notification.md`; it is the spec. In short, two observe-only events in `HOOK_EVENTS`: no blocking, stdout not read, exit codes and timeouts only produce a recent-runs entry and never fail closed, default timeout 60 s, run in the background.
- `StopFailure`: once per turn that ends `Failed` on a model error (the driver's `on_model_error` path, `crates/radian-runtime/src/driver.rs`). Not for hook, compaction or journal errors, not for cancel. `matcher` is the `ModelErrorKind` closed set read from `MODEL_ERROR_KINDS` (`transport`, `protocol`, `unavailable`, `usage_limit`, `context_overflow`, `empty_response`), `*`/empty match all; a matcher outside the set warns once at load and that entry never runs. stdin: `hook_event_name`, `cwd`, `session_id`, `error` (the kind), `error_details` (message, 2000 chars), `last_assistant_message`. `exec -p` waits at most 5 s for it before exiting, then kills the process group; the exit code and output are unchanged.
- `Notification`: `matcher` closed set `permission_prompt`, `idle_prompt`. `permission_prompt` fires each time radian is about to ask a human (TUI modal, `exec` terminal prompt), after a PermissionRequest hook gave no decision. `idle_prompt` fires once when a TUI turn ended and 60 s (a constant) passed with no input; input restarts the timer; not while `/loop` or `/goal` scheduled the next turn; never in `exec`. stdin: `hook_event_name`, `cwd`, `session_id`, `notification_type`, `title: "radian"`, `message` (`radian needs your permission to use <tool>` / `radian is waiting for your input`), never tool arguments or text.
- `async` on these events is accepted and inert.
- import-claude imports both events; StopFailure matchers are translated through one table from Claude's category names to radian's kinds, and an entry that does not map is dropped with a report line.

## Edge cases to cover
1. `exec -p` with a provider returning 429 → `StopFailure` with `error: "usage_limit"`; a `usage_limit` matcher runs and a `transport` matcher does not; exit code identical to the run without hooks.
2. A StopFailure hook that sleeps 30 s → `exec` exits in about 5 s and the hook's process group is gone.
3. Cancelled turn, hook-error turn, Completed turn → no StopFailure.
4. Matcher `rate_limit` → load warning, never runs.
5. TUI ask modal → one `permission_prompt` with the tool name and without its arguments; an ask answered by a PermissionRequest hook → none.
6. Idle 60 s → one `idle_prompt`; 60 s more → none; input in between restarts the timer; `/loop` active → none. Use an injectable clock, never sleep 60 s.
7. A Notification hook that hangs → the modal still appears at once.

## Docs
`docs/user/hooks.md`: one section per event with when it fires, stdin, every matcher value, cannot block, timeout, the 5 s `exec` wait and the 60 s idle constant, and the Claude category mapping table. Then this backlog item is done: ADR 0045 §1 lists the new events (docs-only commit), and delete `docs/backlog/hooks-stopfailure-notification.md` and its README row. Mutation rows in the crates you change.

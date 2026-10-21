# ADR 0011: Durable Inbox Semantics

Status: accepted

## Background

The public interface exposes only semantic operations, never a generic array splice. The runtime inbox carries four operations — `followup`/`steer`/`inject`/`cancel` — and turns inputs into recoverable facts through the journal and the transcript.

## Decisions

### L1: Four semantic operations

| Operation | Lane | Wake | Meaning |
|---|---:|---:|---|
| `followup(input)` | `next_turn` | yes | New user or system task for the turn after the current one. |
| `steer(input)` | `next_step` | yes | Change the direction of the current turn as soon as possible. |
| `inject(context)` | `next_step` | no | Context that waits for a natural step boundary. |
| `inject_pending_wake(context)` | `next_step` | conditional | Context while the turn runs. When it still waits in the queue at turn end, it opens a new turn by itself (work-completion notices only). |
| `cancel(cause)` | no lane | yes | Cancel the in-flight model or tool work. |

The interface exposes only these semantic operations. It exposes **no** generic array `splice`.

- `steer` passes through the Core `SteeringHandle` and enters the transcript at a safe point (after a tool batch, before model end, before the first model request). `steer` accepts only user messages. Wake means: when the driver is idle or waiting, steer advances the next step early. It does not affect in-flight requests.
- `followup` is "input for the next turn": it queues at the turn boundary and becomes the input of the next `start_turn`, FIFO.
- `inject` shares the lane with `steer` but differs in wake: both enter later in the current turn, but only steer actively wakes.
- `inject_pending_wake` shares the lane with `inject` and equally never wakes directly. It differs only at the turn boundary: while the turn absorbs it in some step, it behaves exactly as `inject`. When it still waits in the queue at turn end, it opens a new turn by itself per L5 below (work-completion notices only, ADR 0037 decision 5).
- `cancel` uses no lane and acts directly on in-flight model/tool work. Repeated cancellation is idempotent (a second call neither fails nor produces a second effect).

### L2: Input sources

`InputSource` has seven values in snake_case: `User`, `SlashCommand`, `PermissionDecision`, `Scheduler`, `Mcp`, `Subagent`, `System`.

- The Core layer holds `source: Option`. `None` means "the host did not state a source". It does **not** equal `User`. Core makes no permission decision from it.
- At the runtime **`followup`/`steer`/`inject` boundary, `source` is required**. `None` is not accepted, so the permission layer always receives a source. The value written into `InboxEnqueued.source` is the caller-declared value.
- The permission layer treats `source == Some(User)` as the only marker of human input. `None` and any non-`User` value never carry `User` semantics. An externally queued or externally injected message without a `User` source never gains user-level permissions.
- The effective source of one turn takes the least-privileged of all inputs admitted into it. It only lowers, never raises. It resets at turn end. Its scope covers only inbox-admitted inputs, never tool results (ADR 0011 L2).

### L3: Crash recovery

Input events order as `InboxEnqueued → InputClaimed → InputAdmitted` (ADR 0006 §5.3). Recovery rules:

1. **The transcript is the authority for "already admitted"**: any input whose `meta.id` already appears in a `MessageAppended` message counts as admitted. The reader fills the missing `InputAdmitted` (with the transcript index as reference), then opens the session. It never **replays**.
2. **`InputClaimed` without `InputAdmitted`, and no transcript message with that id**: return to the head of the original lane (not lost, not duplicated, not yet executed: "enter the transcript" is the prior fact before tool side effects).
3. **A tool call already exists without a result**: **never replay a potentially side-effecting tool automatically**. Follow the ADR 0002 `repair` (`Settled` present → original result; `Started` only → `outcome_unknown_after_crash`; no marker → `not_started_after_crash`). Write the recovery results back as `Message` records before reopen.

`InboxEnqueued`/`InputClaimed`/`InputAdmitted` writes belong to runtime. Core has no inbox concept.

### L4: Bounded input

- When the queue is full, enqueue **fails with an explicit error and returns the original message**. The UI keeps the user input and allows retry. The error must carry the rejected message itself, not only an error code. **No message is ever silently lost.**
- `steer` keeps FIFO in the same lane. It performs no dedup by target. Every received message stays auditable.
- `cancel` takes no queue slot and has no capacity limit. Repeated cancellation is idempotent.

### L5: An idle wake entry opens the next turn by itself

`InboxEntry` records whether it wakes (`wake: bool`: steer true, inject false). `InboxEnqueued.wake` persists it (ADR 0006 §5.3). When the driver looks for the next-turn input in `run_loop`:

1. A non-empty `next_turn` wins as before.
2. An empty `next_turn` plus a `wake = true` entry in `next_step` takes the **oldest wake entry** as the first input of a new turn (through `start_turn_with` as with followup). Older inject entries still wait in `next_step`. The first step of the new turn absorbs them through the normal L1 path.
3. At turn end (after `requeue_unadmitted`, before return to wait), first check the inbox through the same two rules. When input exists, open the next turn at once with no further waker. This rule covers normal ends and model-error ends (ADR 0017).

`inject` **never** opens a turn by itself. `inject_pending_wake` (`pending_wake = true`) is the only exception (see below). An old journal with neither `wake` nor `pending_wake` fields reads `next_step` entries as `wake = false`, `pending_wake = false`: the present-day `inject` semantics, with unchanged recovery semantics. Such a turn is a **new turn**, not a continuation of the last one: `TurnFinished` is a durable fact, and continuing the same turn would move turn boundaries and affect ADR 0017/0018/0034 inferences.

### L5 supplement: a work-completion notice still queued at turn end opens a new turn by itself

A work-completion notice queued while the turn runs uses `inject_pending_wake` (`next_step`, `wake = false`, `pending_wake = true`; `InboxEnqueued.pending_wake` persists it, ADR 0006 §5.5). `claim_next_turn` takes first `next_turn`, then `wake = true` `next_step` entries, then the oldest `pending_wake`-marked `next_step` entry. A plain `inject` (unmarked) still never opens a turn by itself.

A turn thus opened **counts as one wake**. An absorbed entry does not count. The host counts in the callback when the inbox claims a marked entry and opens a turn (`JobNotifier::note_promoted_wake`). It never pre-deducts at enqueue time (ADR 0037 decision 5).

### L6: Difference from the configuration boundary

| Dimension | Configuration change (ADR 0004) | Inbox input (this ADR) |
|---|---|---|
| Object | model route / permission tier / thinking | messages/context |
| Effective boundary | turn boundary, after the driver drains the queue | step safe point (steer/inject); turn boundary (followup) |
| In-flight model request | unaffected | unaffected |
| Record point | `ConfigChanged` at apply time | three events at enqueue/claim/admit time |

Configuration switches "what to run with" at the turn boundary. Input decides "where the turn goes next" at step safe points in the transcript. The former answers "what to run with"; the latter answers "where this turn goes next".

### L7: Per-turn replays (`/loop`)

TUI `/loop` resends the same prompt each turn until `--times`/`--for` either reaches its cap:

- Each iteration is a `followup` with fixed source `User` (L2: only a `User` turn receives this session approval grants and memory injection). The message `origin` carries `{"continuation": "loop", "iteration": n}` (ADR 0005 G1: audit data lives in meta, and the journal shows which turns a machine sent).
- The single turn-end decision point sits on journaled `TurnFinished` (the continuation third step `goal-mode` adds input at the same place). `--every D` waiting also decides here: replay starts only when D has passed since the **start** of the last iteration at turn end. A mid-turn deadline never interrupts (it waits for turn end). Multiple due points merge into one.
- Loop replays bypass the ADR 0037 wake rate gate (no `note_wake` call, no rejection by it). The loop's own cap is the gate: `--times`/`--for` either stops it. When neither is given, the default is **20 times or 1 hour, whichever comes first**. Esc/cancellation pauses (drops the prompt, keeps the mode). A model error pauses (keeps the prompt, never retries; `/retry` or a new prompt resumes).
- State stays in-process only: TUI exit ends it. Nothing enters the session or the journal. `radian exec` has no loop. `/loop … prompt` while a turn runs is allowed; the prompt queues to turn end as with `/queue`.

## Constraints and costs

- `cancel` is the only operation outside the queue. Its effect relies fully on the Core cancellation path (in-flight appends wait, cancellation settles, interrupted prefix). Runtime gives no extra guarantee that "cancellation takes effect at some boundary".
- A `followup` queued while a turn runs waits at least until that turn ends. To interrupt the current turn, use `steer` plus `cancel`.
- L3-2 "return to the head of the original lane" requires the recovery side to re-read the lane from `InboxEnqueued.lane`. Relative order in the same lane follows `seq`.

## Verification

Inbox/driver tests in `crates/radian-runtime` cover the four-operation lane/wake mapping, the required `InputSource` with no default, the three-state recovery, the bounded full-queue rejection path, idle wake opening plus inject never opening, old-journal field defaults, and "an input already in the transcript but without an admit record never replays" (including a real `kill -9` CLI test). `docs/schema/fixtures/inbox-wake.jsonl` pins the disk shape. `crates/radian-host` wake-gate tests cover 20 loop replays outside the budget (`source = User` with increasing `iteration`).

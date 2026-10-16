# radian-runtime

The runtime layer above `agent-core` (the turn loop) and `radian-session` (the JSONL
journal): a **durable inbox** and a **per-session driver** (ADR 0011, ADR 0004 path B, ADR
0010).

## The four operations (ADR 0011 L1)

| Operation | Lane | Wakes | Meaning |
|---|---|---|---|
| `Inbox::followup(input, source)` | `next_turn` | yes | A new task for after the current turn. |
| `Inbox::steer(input, source)` | `next_step` | yes | Change the current turn's direction at the core's next safe point. |
| `Inbox::inject(input, source)` | `next_step` | **no** | Context that waits for a natural step boundary. |
| `Inbox::cancel(cause)` | none | yes | Cancels in-flight work; never queued, never subject to capacity, idempotent. |

There is no generic splice/insert API. All three inputs must be user messages, and
**`source: InputSource` is required** at this boundary: the
core keeps `source: Option` (ADR 0005 G2), and the runtime is where "you must say" is
enforced. The source is written into `message.meta.source` and into `InboxEnqueued.source`.

## Bounded input (ADR 0011 L4, decision 1)

The queue is bounded. When full, the three inputs return `InboxError::Full` **carrying the
rejected message** so the host can keep it and retry — nothing is ever silently dropped.
`cancel` is exempt, and a repeated `cancel` before the next turn begins
has no second effect.

## Durability

`Inbox::durable(capacity, journal)` writes `InboxEnqueued` through the session journal
**before** the input is visible in memory: an accepted input is a durable promise. The
driver then writes `InputClaimed` when it claims inputs for a turn and `InputAdmitted`
after the transcript has actually taken the message (using the core's
`TranscriptAppended` notifications, never a transcript diff). All runtime events share the
journal's `seq` counter and fsync path with the core's own records.

## The driver

`Driver::open(agent, journal, inbox, live_events, turn_options)` recovers state from the
file, then `Driver::run()` loops: apply pending configuration, claim the next `next_turn`
input, run one turn, repeat. `Driver::shutdown()` stops it.

- **Turn-boundary configuration** (ADR 0004 path B): `ConfigQueue::push` only queues;
  the driver applies pending changes immediately before `start_turn`, records
  `ConfigChanged`, and updates the `ConfigSlot` the model reads. A change queued mid-turn
  is invisible to that turn, including its later model calls.
- **Step-safe-point input** (ADR 0011 L5): `steer`/`inject` go to the core's
  `SteeringHandle`, so they land after a tool batch or before a model request.
- **Per-turn host additions** (ADR 0021): `set_turn_additions` installs a
  host callback that runs at the start of a turn claimed from a `User`-source input (never for a
  `System` one, such as a job notification), before the turn's first model call, and returns at
  most one message; the driver puts it in the inbox's `inject` lane with `InputSource::System`,
  so it is a durable input with an id that lands right after the user message. A refused inject
  or an empty answer changes nothing — the addition is context, not a session fact.
- **No `ActiveRun` escapes**: the driver owns the session and the run; no public signature
  of this crate mentions `ActiveRun` (the type cannot be named by a caller).
- **Recovery** (ADR 0011 L3): an admitted input is not replayed; every other accepted
  input goes back to the front of its lane in enqueue order; unpaired tool calls go through
  the core's `repair` (ADR 0002) and the repaired messages are written back before the
  session is reopened.
- **Extensions do not survive a restart**: reopening a session re-opens its tool session
  (for MCP, new child processes); their in-process state is gone (plan Phase 2, last point).

## Tests

`tests/queue.rs` (operations, capacity, cancel, wake), `tests/driver.rs` (steer at a safe
point, durable event order and index correspondence, turn-boundary config, the per-turn host
addition), `tests/recovery.rs`
(the three recovery states, a genuinely half-written line, a traversal over every prefix of
a real journal), `tests/e2e.rs` (a turn, a restart, and the next turn). `MUTATIONS.md`
records the mutation checks.

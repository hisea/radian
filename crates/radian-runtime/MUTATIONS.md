# `radian-runtime` mutation record

Base: `e728916` · one-shot worktree `/tmp/runtime-mut` (deleted after) · every mutation run
with `cargo test -q -p radian-runtime --no-fail-fast`; `git checkout --` after each;
`git status --short` empty at the end.

These are the **queue-level** mutations. The durable-level mutations named in the task brief
(`InputAdmitted` written before the transcript, an admitted message replayed, config applied
mid-turn) cannot run yet — the driver and the durable writes are blocked on the
`radian-session` API described in `BLOCKER-durable-events.md`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | full inbox silently drops the input (returns a fake id instead of `Err`) | `a_full_inbox_refuses_and_returns_the_rejected_message` | ✅ failed |
| M2 | `cancel` occupies the queue (advances the enqueue `seq` / consumes capacity) | `cancel_ignores_capacity_and_is_idempotent` | ✅ failed |
| M3 | a repeated `cancel` produces a second effect (generation incremented every call) | `cancel_ignores_capacity_and_is_idempotent` | ✅ failed |
| M4 | `inject` wakes the driver (passes `wake = true`) | `inject_does_not_wake_an_idle_driver_but_followup_and_steer_do` | ✅ failed |
| M5 | `followup` does not wake the driver | same | ✅ failed |
| M6 | `steer` enqueues into `NextTurn` instead of `NextStep` | `the_four_operations_map_to_the_documented_lanes` | ✅ failed |
| M7 | lanes push to the front (FIFO becomes LIFO) | `lane_order_is_fifo_by_enqueue_seq`, `the_four_operations_map_to_the_documented_lanes` | ✅ failed |

7/7 caught, none equivalent.

## Notes

- M1's mutation had to return a *value* rather than `Err`, so it also had to fabricate an
  id; the test catches it on the `Full` variant, not on the fabrication.
- M2 was written as two edits (an extra `seq` advance plus a guard change) because
  `cancel` must be made to *observably* occupy the queue; a single edit would not have
  changed any assertion.

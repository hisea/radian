# `radian-runtime` mutation record

Two passes, both in one-shot worktrees (`/tmp/runtime-mut`, `/tmp/runtime-mut2`), every
mutation run with `--no-fail-fast`, `git checkout --` after each, `git status --short`
empty at the end.

## Queue semantics (`/tmp/runtime-mut`, base `e728916`)

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | full inbox silently drops the input (returns a fake id instead of `Err`) | `queue::a_full_inbox_refuses_and_returns_the_rejected_message` | ✅ failed |
| M2 | `cancel` occupies the queue (advances the enqueue `seq` / consumes capacity) | `queue::cancel_ignores_capacity_and_is_idempotent` | ✅ failed |
| M3 | a repeated `cancel` produces a second effect (generation incremented every call) | same | ✅ failed |
| M4 | `inject` wakes the driver (passes `wake = true`) | `queue::inject_does_not_wake_an_idle_driver_but_followup_and_steer_do` | ✅ failed |
| M5 | `followup` does not wake the driver | same | ✅ failed |
| M6 | `steer` enqueues into `NextTurn` instead of `NextStep` | `queue::the_four_operations_map_to_the_documented_lanes` | ✅ failed |
| M7 | lanes push to the front (FIFO becomes LIFO) | `queue::lane_order_is_fifo_by_enqueue_seq`, `queue::the_four_operations_...` | ✅ failed |

## Durability, recovery and configuration (`/tmp/runtime-mut2`, base `ddc0c38`)

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M-D1 | `InputAdmitted` is written without waiting for the transcript (a fabricated `transcript_index`) | `driver::durable_event_order_and_index_correspondence` (index must equal the `MessageAppended.index`), `driver::steer_applies_after_the_tool_batch` | ✅ failed |
| M-D2 | recovery requeues inputs that were already admitted | `recovery::an_admitted_input_is_not_replayed`, `recovery::crash_traversal_recovers_every_prefix` | ✅ failed |
| M-D3 | configuration is applied mid-turn (right after `start_turn` instead of at the boundary) | `driver::a_config_change_is_not_seen_by_the_turn_that_queued_it` | ✅ failed |
| M-D4 | runtime events are mapped into `journal_records()` (they would enter `replay`/`repair`) | `runtime_events::runtime_events_never_enter_the_transcript_fold` | ✅ failed |
| M-D5 | a runtime event's `critical` is hard-coded `false` instead of coming from the type | `runtime_events::every_runtime_event_writes_its_documented_critical_flag` | ✅ failed |
| M-D6 | `append_runtime_event` does not advance `seq` (the two counters collide) | `runtime_events::runtime_events_keep_seq_continuous_and_do_not_touch_the_message_index` | ✅ failed |

13/13 caught, none equivalent. The brief's list is covered: "queue full silently drops" (M1),
"`cancel` occupies the queue" (M2), "a repeated cancel has two effects" (M3), "`InputAdmitted`
before the transcript" (M-D1), "an admitted message is replayed" (M-D2), "`inject` wakes the
driver" (M4), "configuration applies immediately" (M-D3), plus the unblock brief's "`critical`
from the caller" (M-D5), "runtime events in `journal_records()`" (M-D4) and "the two counters
collide" (M-D6).

## Notes

- M1 had to fabricate an id to return `Ok`; the test catches it on the `Full` variant.
- M2 was two edits (an extra `seq` advance plus a guard change) because `cancel` must be made
  to *observably* occupy the queue.
- M-D3 was two edits (remove the boundary call, add a mid-turn call) for the same reason.

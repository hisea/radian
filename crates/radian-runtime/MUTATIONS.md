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

## ADR 0015 resume 基线 + BudgetRelax 清理（`/tmp/radian-mut`，base `b9c43c2`）

一次性检出 `git worktree add --detach HEAD`（HEAD = `b9c43c2`），每个变异用 `git checkout --`
恢复、`git status --short` 为空；全部 `cargo test -p radian-runtime --test budget --no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M-B1 | `Driver::open` 不再调用 `restore_budget_baseline`（resume 后只能估算） | ✅ failed (rc 101) | `resume_restores_the_reported_baseline` |
| M-B2 | `reported_baseline` 让**第一条**带 `input` 的报告胜出（`baseline.is_none()` 才写） | ✅ failed (rc 101) | `resume_uses_an_earlier_report_when_the_last_has_no_input` |
| M-B3 | `BudgetRelax::take` 用 `load(false)` 代替 `swap(false)`（标志永不清除） | ✅ failed (rc 101) | `relaxing_once_sends_the_blocked_request_and_restores_the_guard` |

M-B3 正是本切片补的洞：原测试只跑两轮，`load` 变异照样绿。第三轮（不重新 `relax_once`）让
「标志泄漏到下一轮」可观察，才抓住它。M-B2 的变异是「取错那次报告」的实例（first-wins）；取
「最后一条报告」而不要求它带 `input` 的变异由同一测试覆盖（输出-only 报告若被当成 0，投影与
基线就不是 900）。

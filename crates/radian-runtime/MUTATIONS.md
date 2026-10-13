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

## 模型错误只结束这一轮（ADR 0017，2026-10-13）

一次性 worktree `/tmp/resil2-mut`（从 `159489f` 创建，用完删除，**不共享 target 目录**），
`--no-fail-fast`；每个变异后从副本还原，`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M1 | `is_fatal` 恒为 `true`（模型错误仍让 driver 结束） | ✅ 抓住 (rc=101) | `driver::tests::only_model_errors_are_non_fatal` |
| M2 | 模型错误路径不写 `TurnFinished` | ✅ 抓住 (rc=101) | `provider_failure::a_failed_model_request_ends_the_turn_and_writes_turn_finished` |
| M3 | adapter 的 `mask` 改成恒等（`TurnFailed.message` 带上 key） | ✅ 抓住 (rc=101) | `provider_failure::the_turn_failed_message_never_contains_the_key` |
| M4 | `is_fatal` 把 `Journal` 也算非致命 | ✅ 抓住 (rc=101) | `driver::tests::only_model_errors_are_non_fatal` |

M3 改的是 `agent-provider-anthropic/src/model.rs` 的 `mask`（与上一轮 M25 同一处），本轮由新的
**落盘记录**测试抓住：`TurnFailed.message` 会写进会话文件，因此它不能含 key。

## 已 claim 未 admit 的 steer 要回队（ADR 0011，2026-10-13）

一次性 worktree `/tmp/resil3-mut`（从 `c0640b7` 创建，用完删除，**不共享 target 目录**），
`--no-fail-fast`；变异后从副本还原，`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M5 | 去掉 `requeue_unadmitted` 调用（回到旧行为：claim 了但没应用就丢） | ✅ 抓住 (rc=101) | `steering_after_failure::a_steer_accepted_during_a_failing_turn_is_requeued_and_used_once` 与 `..._max_tokens_finish_...`（两条都失败） |

M5 同时证明"必须先在旧代码上失败"：去掉 requeue 就是旧代码，两条测试都失败。

## hook 错误也非致命（ADR 0017 第三轮，2026-10-13）

一次性 worktree `/tmp/hookerr-mut`（从 `7baa68e` 创建，用完删除，**不共享 target 目录**），
`--no-fail-fast`；变异后从副本还原，`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M6 | `is_fatal` 把 `Hook` 改回致命 | ✅ 抓住（unit rc=101；e2e rc=101，看门狗 5s 触发） | `driver::tests::only_model_and_hook_errors_are_non_fatal` 与 `hook_failure::a_before_model_hook_error_ends_the_turn_and_the_session_continues` |
| M7 | `is_fatal` 把 `Journal` 也算非致命 | ✅ 抓住 (unit rc=101) | `driver::tests::only_model_and_hook_errors_are_non_fatal` |

**两条变异未运行，因为对应的修复代码不存在**（brief 要求的另外两条）：

- "hook 失败不补结果"、"已执行的调用也被覆盖成错误" —— 前提不成立。复核发现
  `before_tool_call`/`after_tool_call` 的错误**不结束本轮**（fail closed：给该调用一条
  `hook_error` 结果并继续），而会 `RunError::Hook` 的两个点（`before_model_call` /
  `after_model_response`）都在本步 assistant 消息 commit **之前**跑，本批次没有未完成的调用。
  所以没有"补结果"的代码可变异；正确性由 `agent-core::hooks` 的两条测试与
  `hook_failure` 的端到端测试钉住（transcript 每个 tool_call 都有结果）。

## ADR 0018：不再写 `ContextBudgetReached`（`pi/guard`）

一次性 worktree `/tmp/radian-guard-mut`（HEAD = `8825691`，独立 target），`--no-fail-fast`；
变异后 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M8 | `run_turn` 在 `TurnFinished` 前重新写 `ContextBudgetReached` | ✅ caught (rc 101) | `an_over_budget_turn_completes_and_writes_no_context_budget_event`（`assert_no_context_budget_event` 发现事件） |

历史值仍可读：`a_historical_budget_stop_still_reads_and_resumes` 手写一份旧格式会话
（`TurnOutcome::ContextBudget` + `ContextBudgetReached`），`Session::open` 能读，`Driver` 能 resume。

# Mutation checks — ADR 0018 automatic compaction (the pure planning half)

Run in a one-time worktree (`git worktree add --detach /tmp/radian-cplan-mut HEAD`) against
commit `728e2b8`, with its **own `CARGO_TARGET_DIR`**, using `cargo test -p agent-core --test
compaction --no-fail-fast`. Each mutation was restored with `git checkout --` and `git
status --short` was clean before moving on.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M1 | `plan` accumulates per message instead of per atomic unit, so a cut can land inside a parallel tool call | caught (rc 101) | `plan_cuts_only_at_a_unit_boundary` |
| M2 | `plan` drops an oversized last unit (`NothingToCompact`) instead of keeping it whole | caught (rc 101) | `an_oversized_last_unit_is_still_kept_whole` |
| M3 | `truncate_chars` returns the whole tool result (no 2000-character cut) | caught (rc 101) | `the_summary_request_serializes_text_and_truncates_tool_results`, `truncating_a_tool_result_never_splits_a_character` |
| M4 | `summarize_text` includes an assistant's `reasoning` | caught (rc 101) | `the_summary_request_serializes_text_and_truncates_tool_results` |
| M5 | `keep_target`'s max/min are swapped (`quarter.max(MAX).min(MIN)`) | caught (rc 101) | `keep_target_clamps_the_quarter_of_the_budget` |
| M6 | `compacted_history` skips the `validate_history` call | caught (rc 101) | `compacted_history_refuses_an_invalid_kept_suffix` |
| M7 | `units` makes each tool result its own unit instead of part of its assistant's unit | caught (rc 101) | `an_assistant_and_all_its_results_are_one_unit` |

Every mutation was caught; none was equivalent. The randomized
`plan_and_compaction_are_valid_for_many_histories` (200 seeds) is not a mutation target on
its own — it is the broad validity net under `units`, `plan` and `compacted_history`.

## compact-core：自动压缩的编排（ADR 0018 §B）

Base: `0f5d5e2` · 一次性检出 `/tmp/ccore-mutB`（detached，跑完删除）· 每个变异跑完即
`git checkout -- .`，结束时 `git status --short` 为空 · `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M2 | `replace_transcript` 提交后不清 `budget_baseline` | `compaction_flow::a_committed_compaction_clears_the_budget_baseline` | ✅ failed |
| M3 | `T_after` 用旧基线投影（`project(budget, self.run.session.budget_baseline, …)` 取代 `None`） | `compaction_flow::a_committed_compaction_clears_the_budget_baseline` | ✅ failed |
| M4 | 摘要调用也 `model_calls += 1` | `compaction_flow::a_request_over_the_budget_is_compacted_and_committed` | ✅ failed |
| M5 | 摘要无效不重试（`for attempt in 0..1`） | `compaction_flow::an_invalid_summary_is_retried_once` | ✅ failed |
| M6 | `below_target` 后仍自动压缩（`if false && …`） | `compaction_flow::a_below_target_compaction_stops_auto_compaction_for_the_turn` | ✅ failed |
| M7 | `CompactionUsage` 不写（只在提交时才写的方向；这里整个不写） | `compaction_flow::a_replacement_abort_keeps_the_usage_and_recovers_the_old_history` | ✅ failed |
| M8 | `TranscriptFold` 接受待定段内的 `ContextCompacted` | `compaction_flow::a_compaction_record_inside_a_pending_segment_is_corruption` | ✅ failed |
| M10 | `replace_transcript` 不更新 `transcript_revision` | `compaction_flow::a_request_over_the_budget_is_compacted_and_committed` | ✅ failed |

8/8 抓住。M2 最初漏过：当时测试只看提交后 `budget_status` 的来源，而那个报告来自压缩后的
`report_after`，与 `budget_baseline` 字段无关。补上 `testing_budget_baseline_input()` 接缝后，
测试直接断言基线被清空，M2 才被抓住。M7 用“整个不写 usage”作近似（把写入移到提交之后的
改动无法用单次替换表达），`a_replacement_abort_*` 断言中止路径仍有 `CompactionUsage`，因此仍
指向“usage 必须在中止前写”。

## compact-overflow：超长兜底与手动 `/compact`（ADR 0018 §兜底、手动与关闭）

Base `dc307e6` · 一次性检出 `/tmp/covf-mut`（从 HEAD 建，跑完删除）· **独立
`CARGO_TARGET_DIR`** · 每个变异跑 `cargo test -p agent-core --test compaction_overflow`、
`cargo test -p radian-runtime --test compact_control`、`cargo test -p radian-tui --lib`、
`cargo test -p agent-cli --test cli an_interactive_compact`（都 `--no-fail-fast`），然后
`git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 兜底不设“只重试一次”上限（去掉 `overflow_retried` 的失败分支） | `compaction_overflow::a_retry_that_still_overflows_fails_after_one_retry` | ✅ failed |
| M2 | 非超长错误也触发兜底（去掉 `ContextOverflow` 的模式约束） | `compaction_overflow::a_non_overflow_model_error_does_not_compact` | ✅ failed |
| M3 | 兜底目标用 0.75 而不是 0.5（`4·T_after ≤ 3·reference`） | `compaction_overflow::{a_fallback_that_cannot_reach_half_commits_nothing, a_known_window_uses_min_of_projection_and_budget}` | ✅ failed |
| M4 | 窗口已知时不取 `min(T_before, B)`（`reference = T_before`） | `compaction_overflow::a_known_window_uses_min_of_projection_and_budget` | ✅ failed |
| M5 | 兜底重发计入 `model_calls` | `compaction_overflow::an_overflow_with_no_window_compacts_to_half_and_retries_once` | ✅ failed |
| M6 | 手动 `/compact` 把说明写进 transcript（`commit`） | `compact_control::compact_between_turns_commits_and_enqueues_nothing` | ✅ failed |
| M8 | 手动 `/compact` 也要求 `T_after ≤ B`（`Gain::Any` → `Gain::Budget`） | `compaction_overflow::compact_now_only_requires_a_smaller_request` | ✅ failed |

6/6 抓住。**“手动压缩在唤醒分支执行”无法用编译得过的变异表达**：`run_turn` 期间 `ActiveRun`
持有 `&mut self.session`，`Driver::run_control` 同样需要 `&mut self`，借用检查器从结构上排除了
“turn 运行中执行控制请求”。该不变量由顺序测试 `compact_control::compact_waits_for_a_running_turn`
（`TurnFinished` 的 `seq` 必须小于 `ContextCompacted`）与既有的唤醒分支死锁测试
（`compaction_gate`）共同钉住。

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

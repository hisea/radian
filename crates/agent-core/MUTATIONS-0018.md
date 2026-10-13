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

## sumlen：摘要上限与超长重试（dogfood7）

Base: `4ff7528` · 一次性检出 `/tmp/sumlen-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S1 | `summary_system_prompt` 不写上限（只返回基础提示） | `compaction::the_summary_request_states_the_length_ceiling` | ✅ failed |
| S2 | 超长重试仍发原请求（不发缩短请求） | `compaction_flow::an_overlong_summary_is_shortened_on_the_retry` | ✅ failed |
| S3 | 缩短目标用 100% 而非 80% | `compaction_flow::an_overlong_summary_is_shortened_on_the_retry` | ✅ failed |
| S4 | 空摘要也走缩短请求 | `compaction_flow::an_empty_summary_resends_the_original_request` | ✅ failed |

4/4 抓住。

## sumcap：摘要上限随窗口缩放（dogfood9）

Base `ccc3af1` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| C1 | `summary_max_tokens` 恒返回 8192（旧行为） | `compaction::the_summary_ceiling_scales_with_the_window`、`the_scaled_ceiling_reaches_the_prompt_and_the_check`、`compaction_flow::the_auto_path_scales_the_summary_ceiling_with_the_window`、`compaction_overflow::the_fallback_scales_the_summary_ceiling_with_the_window`、`compact_now_scales_the_summary_ceiling_with_the_window`（并连带 `an_overlong_summary_is_shortened_on_the_retry`、`two_overlong_summaries_fail_the_turn`，因为小 `B` 时 8192 ≠ `K`） | ✅ failed |
| C2 | `B/8` 写成 `B/4`（超过 `K` 时被 `min` 截住） | `compaction::the_summary_ceiling_scales_with_the_window`（`103616` → 20000 ≠ 12952）、`the_scaled_ceiling_reaches_the_prompt_and_the_check`、三条路径各自的缩放测试 | ✅ failed |
| C3 | 超长兜底路径漏改（`summary_budget: None`） | `compaction_overflow::the_fallback_scales_the_summary_ceiling_with_the_window` | ✅ failed |

3/3 抓住。C2 只在“`B/8` 超过 8192 且 `B/4` 被 `K` 截住”时与正确值不同，所以只有大窗口（`B=103616`）
的断言能区分；小窗口（`B=16000`）那条不区分，正如 brief 所要求。

## sumsect：摘要是为了「接着干」（方案 B / dogfood9）

Base `3d7066b` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| V1 | `SUMMARY_SYSTEM_PROMPT` 删掉 `## Verified facts (do not re-check)` 整节 | `compaction::the_summary_request_serializes_text_and_truncates_tool_results` | ✅ failed |
| V2 | 删掉「计划不算」的限制语（`SUMMARY_PLANS_ARE_NOT_FACTS` 不再出现在提示词里） | `compaction::the_summary_request_serializes_text_and_truncates_tool_results` | ✅ failed |
| V3 | `shorten_request` 的正文去掉「保留 Verified facts」那句 | `compaction::the_shorten_request_carries_only_the_overlong_summary`、`compaction_flow::an_overlong_summary_is_shortened_on_the_retry` | ✅ failed |

3/3 抓住。测试断言用的是 `SUMMARY_PLANS_ARE_NOT_FACTS` / `SUMMARY_SHORTEN_KEEPS_FACTS` 两个稳定子串
常量，不绑定整段措辞；V2/V3 因此精确地落在「规则是否还在」上。

## sumsect-2：Verified facts 的可执行删除优先级与三分之一上限

Base `faeae09` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| W1 | `verified_facts_max_tokens` 的除数 `3` 改成 `2` | `compaction::the_summary_prompt_states_the_verified_facts_ceiling`、`the_shorten_request_carries_only_the_overlong_summary` | ✅ failed |
| W2 | `shorten_request` 正文漏掉 `Verified facts` 三分之一上限 | `compaction::the_shorten_request_carries_only_the_overlong_summary`、`compaction_flow::an_overlong_summary_is_shortened_on_the_retry` | ✅ failed |
| W3 | `shorten_request` 正文漏掉删除优先级与时间顺序 | `compaction::the_shorten_request_carries_only_the_overlong_summary`、`compaction_flow::an_overlong_summary_is_shortened_on_the_retry` | ✅ failed |
| W4 | `SUMMARY_SYSTEM_PROMPT` 漏掉时间顺序规则 | `compaction::the_summary_request_serializes_text_and_truncates_tool_results` | ✅ failed |

4/4 抓住。测试断言用 `SUMMARY_FACTS_ORDER_RULE` / `SUMMARY_FACTS_DELETE_OLDEST` 两个稳定子串常量，
W3/W4 因此分别落在 shorten 正文与系统提示上。

## 修订 6：用户原话的逐字保留（2026-10-13）

Base `0a1c18b` · 一次性检出 `/tmp/vb-mut`（detached，跑完删除）· 独立 target ·
`--no-fail-fast`；每个变异后从副本还原，结束时 `git status --short` 只剩变异脚本。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| V1 | 摘要请求的 `previous_summary` 用摘要消息的**整段正文**（含上一份原话） | ✅ 抓住 | `compaction_flow::the_summary_request_does_not_carry_the_previous_verbatim_list` |
| V2 | `select_user_messages` 丢掉第一条 | ✅ 抓住 | `compaction::the_first_message_is_kept_and_cut_to_u1`、`compaction_flow::the_first_user_message_survives_eleven_rolling_compactions` |
| V3 | `U` 的填充顺序改成从旧到新 | ✅ 抓住 | `compaction::the_rest_fill_u_from_the_newest_with_the_omitted_count`（最新一条必须保留） |
| V4 | plan 不扣原话的实际大小 | ✅ 抓住 | `compaction_flow::the_plan_reserves_the_verbatim_space` |
| V5 | 不再按 `source` 过滤（`SlashCommand` 等也收） | ✅ 抓住 | `compaction::only_user_messages_are_retained`、`the_summary_message_is_not_retained` |
| V6 | 附件正文直接进原话（不换占位行） | ✅ 抓住 | `compaction::an_attachment_becomes_a_placeholder` |
| V7 | 读回解析正文、不读 `origin` | ✅ 抓住 | `compaction::the_verbatim_messages_are_written_and_read_back_from_origin`（正文里带同样的 marker/heading 的消息会被解析错） |
| V8 | 去掉 `U` 的 512 下界 | ✅ 抓住 | `compaction::the_verbatim_budget_is_the_two_layer_rule`、`a_second_compaction_does_not_stack_omission_markers` |
| V9 | `U₁` 固定 2000、不随 `U` 缩小 | ✅ 抓住 | `compaction::the_verbatim_budget_is_the_two_layer_rule` |

9/9 抓住。V4 与 V7 起初**没被抓住**：V4 缺一条"plan 为原话留量"的流程测试，V7 的
round-trip 测试里没有"正文含同样 marker"的消息，于是补了
`the_plan_reserves_the_verbatim_space`，并把 round-trip 的第二条改成
`--- user message 9 ---` 与 heading 都在正文里的消息，两者随即被抓住（补测在
`0a1c18b`）。

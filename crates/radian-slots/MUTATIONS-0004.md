# 变异记录：ADR 0004 / `crates/radian-slots`（Phase 0 / H）

日期：2026-10-12。全部变异在一次性检出 `/tmp/h-slots-mut`（`git worktree add`，HEAD = 本提交）里做，每项变异后跑对应测试、恢复源码，提交前 `git status --short` 为空（已核对）。看门狗型断言：`run_ids_isolate_concurrent_turns_sharing_one_slot` 的 `wait_entered` 包在 `tokio::time::timeout(10s)` 里，把"错误 pin 结构导致的死锁"变成干净的失败断言。

| # | 变异 | 应抓住的行为 | 结果 |
|---|---|---|---|
| M1 | `ModelSlot::swap` 只记录不替换 adapter | swap 后下一次请求用新 adapter | ❌ `model_slot_is_a_model_and_erases_two_distinct_adapter_types`、`swap_during_an_inflight_request_leaves_it_on_the_old_adapter`、`swap_mid_turn_affects_the_same_turns_next_model_call` 全部 FAILED |
| M2 | `PolicySlot::swap` 只记录不替换 policy | swap 后下一次决策用新策略 | ❌ `policy_slot_denies_then_allows_after_a_swap`、`policy_swap_does_not_affect_an_inflight_decision` 全部 FAILED |
| M3 | `ConfigLog::record` 用 `load` 而非 `fetch_add` | revision 单调递增 | ❌ `shared_log_keeps_one_monotonic_revision_sequence` FAILED（[0,0,0]） |
| M4 | 去掉 `#[serde(tag = "kind")]` | `ConfigChanged` 的序列化形状 | ❌ `config_changed_round_trips_through_json`、`old_reader_ignores_unknown_optional_fields` FAILED |
| M5 | `ConfigChanged` 加 `#[serde(deny_unknown_fields)]` | 未知可选字段被忽略 | ❌ `old_reader_ignores_unknown_optional_fields` FAILED |
| M6a | `PinnedModelSlot::complete` 忽略 run id（总是 `current()`） | run 标识把路由固定一个 turn | ❌ `pinned_slot_keeps_route_across_the_model_calls_of_one_turn`、`run_ids_isolate_concurrent_turns_sharing_one_slot` FAILED（`without_a_run_scope…` 通过，符合预期） |
| M6b | pin 表改成**单共享胞**（无 run 键，后写覆盖） | 并发 turn 各自固定自己的路由 | ❌ `run_ids_isolate_concurrent_turns_sharing_one_slot` FAILED（turn 2 拿不到独立 pin，看门狗 10s 断言失败；单 turn 测试仍过） |
| M6c | `ModelSlot::complete` 把 `self.current()` 移进 async 体内（延迟捕获） | swap 不影响在飞请求 | ⚠️ 无测试失败 → **等价变异**：async 体在首个 poll 就执行捕获，swap 发生在 adapter 内 park 期间，行为不变。在飞保护是结构保证（Arc 在首个 poll 捕获、锁不跨 await），由此确认没有不悬挂的代码变异能破坏它。 |

结论：M1–M6b 全部被抓住；M6c 为等价变异（结构保证）。唯一可注意的失败模式是 M6b 原先会死锁（run2 不产生工具调用、`wait_entered(2)` 永远等），已改为看门狗断言使其干净失败。
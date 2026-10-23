# arc：main 变了就自动 reverify，有两道闸防死循环

- 状态：decided
- 涉及 ADR：0048（§9 失败与重试、reverify、§10 调度）

## 背景

一个 task 的 gate 在 `commands` 上失败，原因有时不在 task 本身：可能是 main 上原有一个脆弱的测试，也可能是 main 自身坏了。main 修好之后，这个 task 什么都不用改，重新验一次就能过。今天只有 owner 能手动运行 `radian arc reverify`，夜里没人时 task 就一直停在 `review.rework_pending`（例：TK0026 等 TK0028 修好目录超时测试）。

## 决定（已定）

1. **什么时候自动 reverify。** 以下三条同时成立时，调度循环自动 reverify，效果与 owner 运行 `reverify` 相同：
   - 所在 plan 的 `auto` 不是 `off`；
   - task 处于 `review.rework_pending`，原因是 `commands` 或 integration 阶段的 `command_failed` 或 `timeout`；
   - 远端 main 已不是那份失败 verdict 记录的 `main_sha`。
2. **不自动 reverify 的情况：**
   - review 阶段挡下的；
   - 冲突（由「先合 main」的冲突返工处理）；
   - 拒绝说明；
   - delivery 失败；
   - lead 已写返工、task 已回到 `todo.ready`。
3. **第一道闸：输入不变不重跑。** 每次自动 reverify 记下当时的 `(head_sha, main sha)`。同一组输入最多自动 reverify 一次；main 不动就不会再跑。
4. **第二道闸：次数上限。** 新键 `[limits] max_auto_reverify`，整数，缺省 `2`，取 `0` 或更大，`0` 即关闭，按 task 计数。到达上限后 task 停在 `review.rework_pending`，`run` 打印一行原因，`show` 的 `rework_reason` 也写明。不暂停 plan。
5. **其余规则不变。** reverify 仍不派 worker、不计入 `max_attempts`、不计入 `auto_rework`。`run` 为每次自动 reverify 打印一行，例：`TK… main moved (<短 sha>) -> auto reverify`。
6. **owner 的手动 `reverify` 不受影响。** 它不看两道闸，也不消耗 `max_auto_reverify` 的次数。

## 验收

- 一个 `commands` 失败的 task：main 前进后，下一轮自动 reverify 并通过；main 不动则不跑。
- 同一组输入只自动 reverify 一次；到达 `max_auto_reverify` 后不再跑，并打印原因。
- review 挡下的、冲突的、`auto` 为 `off` 的 task，都不会自动 reverify。
- 手册写明新键（类型、缺省、取值范围、何时生效）、`run` 打印的那一行和 `rework_reason` 的文字。

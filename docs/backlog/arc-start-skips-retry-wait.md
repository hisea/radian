# arc：`start` 不必等重试退避

- 状态：decided（可以派发；很小，一个 worker）
- 涉及 ADR：0048（§1 命令、§9 失败、重试与 stall）
- 来源：一个 attempt 被意外取消后，task 进 `todo.retry_wait`，退避已到 5 分钟上限；owner 想马上重跑，`start` 拒绝，只能等

## 背景

一个 attempt 失败、按规则可以重试时，task 进 `todo.retry_wait`，`retry_at` = 现在 + 退避（第一次 10 秒，每多一个 attempt 翻倍，最多 5 分钟）。`retry_at` 到了才变回 `todo.ready`。`radian arc start <task>` 只接受 `todo.ready`，对 `todo.retry_wait` 退出 3：`task … is todo.retry_wait; only a todo.ready task starts`。

退避是给自动调度用的：避免一个反复失败的 task 在循环里空转。owner 亲手运行 `start`，是一个明确的"现在就跑"，退避在这里只是让人干等。

## 决定（已定）

1. `radian arc start <task>` 也接受 `todo.retry_wait`：不看 `retry_at`，当场领取，与 `todo.ready` 的领取完全相同（同一事务、attempt 加一、同样的并发名额检查）。打印的那一行与普通 `start` 相同。
2. 调度循环（`radian arc run`、`--watch`）不变：照旧只在 `retry_at` 过了之后把 `todo.retry_wait` 变回 `todo.ready` 再领取。
3. `todo.paused_budget`、`failed.*` 仍被 `start` 拒绝，规则不变；`max_attempts` 的判断不变（重试是否允许在进 `retry_wait` 时已经判过）。
4. lead 的返工（MCP `add_rework`）今天已经直接回到 `todo.ready`，不受退避影响，不改。

## 验收

- 一个 `todo.retry_wait`、`retry_at` 在将来的 task：`start` 当场领取并运行；同样的 task 在一次性 `run` 里不被领取，直到 `retry_at` 过去。
- 并发名额已满时，`start` 对 `todo.retry_wait` 的 task 照旧退出 3，状态不变。
- `todo.paused_budget` 与 `failed.*` 的 task，`start` 照旧退出 3。
- 手册 `docs/user/arc.md` 的 `start` 一节与状态表写明这一条。

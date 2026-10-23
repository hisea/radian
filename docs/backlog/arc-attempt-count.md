# arc：`max_attempts` 不算返工轮次

- 状态：decided（owner 定：返工不计入 `max_attempts`；冲突返工的连续上限是 lead 的补充，待 owner 看过）
- 涉及 ADR：0048（§9 失败、重试与返工）

## 背景

`max_attempts`（缺省本仓库配置为 8）今天数的是 task 的全部 attempt：失败后的重试，加上每一次返工（lead 的 `add_rework`、arc 写的 review 返工、冲突返工）。TK0017 被 review 打回了 6 轮，第 8 次 attempt 在 gate 上碰到与 main 的冲突；冲突返工也要算一次，名额用完，task 直接 `failed.exhausted`，而它的分支上是一份已经过 review 的改动。返工是"做的事改了"，重试是"同一件事再跑一遍"；两者共用一个上限，就让 review 轮数挤掉了 arc 自己的修复轮。

## 决定（已定）

1. `max_attempts` 只数**重试**：一个 attempt 失败（worker 失败、stall、wall time、基础设施失败、delivery 失败）后 arc 按重试规则再开的 attempt。由返工开始的 attempt 不计入：lead 的 `add_rework`、arc 写的 review 返工、冲突返工。
2. 每次返工都把重试计数归零：返工之后，新的说明有它自己的 `max_attempts` 次重试。
3. 返工的上限照旧由各自的规则管：`auto` 下的返工由 `max_auto_rework` 管（到了暂停 plan）；lead 手写的返工不设上限（lead 自己负责）。
4. **冲突返工的上限（lead 补充，待 owner 看过）**：冲突返工既不计 `max_auto_rework`、现在也不计 `max_attempts`，一个一直冲突的 task 会无限循环。加 `[limits] max_conflict_rework`（整数，缺省 `3`，`0` 及以上）：**连续**的冲突返工次数到了上限，task 停在 `review.rework_pending`，`run` 与 `show` 说明原因，等 lead；中间任何一次不冲突的 attempt 让计数归零。
5. `attempt` 字段（attempt 编号）照旧每次加一，只是不再拿来跟 `max_attempts` 比。`show` 增加当前的重试计数。

## 验收

- 一个 task 被 review 返工 10 次，每轮都能开始，不会 `failed.exhausted`。
- 返工之后的 attempt 失败，按 `max_attempts` 重试，计数从 0 开始。
- 连续冲突到 `max_conflict_rework` 停在 `review.rework_pending`；中间一次不冲突就归零。
- 手册 `arc-files.md` 的 `limits.max_attempts` 一行改写，新键一行；`arc-gate.md` 的失败与返工段落改写。

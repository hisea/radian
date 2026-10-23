# arc：死掉的 gate 不再无限重启，`reverify` 能强制重跑

- 状态：decided（owner 定：做这两件事，2026-10-23；下面的设计是 lead 的）
- 涉及 ADR：0048（gate 的记录与 `reverify`）、0044

## 背景

gate 的记录 `runs/<task>/gate.json` 有 `started`、`running`、`done`、`refused`、`error`、`no_record`。gate-no-record 已经落地：gate 一条记录都没写就退出（停在 `started`），下一个 pass 记成 `no_record`，输入不变就不再启动。还剩两个口子：

1. **写过 `running` 之后死掉的 gate 一直重启。** 下一个 pass 看到 `running` 而进程不在了，就再起一个 gate。这条规则本来是给 loop 自己停 gate（SIGINT/SIGTERM、`cancel_grace_secs` 后 SIGKILL）的情况用的，已写的 verdict 会复用。但一个每次都会崩的 gate（stage 里 panic、OOM 被杀）也走这条路，每个 pass 都重启它，无人看管时没有尽头。
2. **`no_record` 之后没有命令能强制重跑。** task 停在 `review.verifying`，只有新的 `head_sha`、新的 main 或改了 `config.toml` 才允许再启动一次。`reverify` 只接受 `review.rework_pending` 和 `review.awaiting_human`，owner 手里没有"现在就重跑"的命令。

## 决定

1. **arc 自己停的 gate 记 `interrupted`。** loop 在 SIGINT/SIGTERM 时停 gate，`cancel` 停 gate，都在 gate 进程结束后把记录改成 `interrupted`（保留 attempt 与输入）。下一个 pass 对 `interrupted` 照旧当场重启，复用已写的 verdict，不计数。
2. **其余的 `running` 死亡计数。** 一个 pass 看到 `running` 而进程已不在（按记录里的 pid 与启动时间判断，与今天相同），就是一次死亡：记录里的 `deaths` 加一后重启。同一个 attempt、同样输入下的死亡数到了 `[limits] max_gate_deaths`（整数，缺省 `2`，`0` 及以上），这个 pass 不再重启，而是把记录写成新状态 `died`，此后与 `no_record` 完全一样：summary 里报告、`show` 的 `rework_reason` 写明原因、输入不变就不启动、这个 pass 不算有变化；新的 `head_sha`、新的 main 或改了 `config.toml` 允许重新开始，`deaths` 从 0 算起。`0` 表示第一次死亡就停。
3. **`reverify` 接受 gate 卡住的 task。** `radian arc reverify <TASK>` 也接受 `review.verifying`，条件是它的 gate 记录是 `no_record` 或 `died`；其他 `review.verifying` 仍以 exit `3` 拒绝。规则与今天的 `reverify` 相同：领一个新的无 worker attempt，当场验证，不计 `max_attempts`，不花钱，不是自动返工。
4. 新状态 `died` 和 `interrupted` 进 gate 记录状态的那一个列表（如果今天没有这样的列表，就建一个，解析与文档漂移测试都读它）。

## 验收

- 一个每次都在写过 `running` 后崩掉的 gate（测试用的环境变量开关，像 `RADIAN_ARC_GATE_TEST_EXIT_WITHOUT_RECORD` 那样只给测试用）：缺省下启动 1 + 2 次后记成 `died`，之后的 pass 不再启动；改 `config.toml` 后允许再来。
- loop 收到 SIGTERM 停掉的 gate 记成 `interrupted`，下一个 pass 重启且不计死亡。
- `no_record` 和 `died` 的 task，`reverify` 当场重跑；一个 gate 正常 `running` 的 `review.verifying` task，`reverify` 退出 `3`。
- 手册：`arc-gate.md` 的 gate 记录段落写全所有状态，`arc-files.md` 新增 `limits.max_gate_deaths` 一行和 `gate.json` 的状态，`arc.md` 的 `reverify` 一节改写可接受的状态。ADR 0048 在同一分支的 docs-only commit 里改写。

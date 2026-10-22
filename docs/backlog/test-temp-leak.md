# 测试泄漏临时目录；gate 开跑前查 inode

- 状态：idea
- 涉及 ADR：0048（gate 的 `commands` 与 `integration` 阶段）
- 来源：arc 第二个 plan 的复盘

## 背景

测试在系统临时目录下建目录，不少用完不删。Linux gate 机的 `/tmp` 是 tmpfs，inode 有上限（那台是 1,048,576）。有一次它被 21.5 万个泄漏目录占到 inode 100%，之后整套测试一大批在 0.006 秒内失败（建不了目录），gate 变红。这一次假红让一个 task 白白用掉一次返工（返工上限 2，再红一次 plan 就暂停）。

清理后几个小时内又涨到约 10.9 万 inode（11%），顶层目录按前缀数：`radian-exec-sessions-*` 约 5,700，`radian-coding-tests*` 约 1,600，`radian-session-tests*` 约 700，`radian-runtime-tests*` 约 330，`radian-exec-exec-jobs-race-*` 三种各 120，其余几十个以下。也就是说每跑一次完整 Verify 就泄漏一批，量随跑的次数线性增长。

## 建议

1. **修泄漏**：测试的临时目录统一走一个会在 `Drop` 时删除的 helper（有的 crate 已有自写的 `TempDir`，有的直接 `create_dir` 后不删）。子进程写的目录（例如 exec 测试里 radian 自己建的 sessions 目录）放在测试自己的临时根下面，随根一起删。
2. **gate 前置检查**：`scripts/verify.sh` 和 `scripts/arc-gate.sh` 开跑前查临时目录所在文件系统的剩余 inode 和空间，低于阈值就以「环境问题」失败并写明原因，而不是让测试成批假红。阈值和是否顺带清理旧目录待定。
3. 一条测试：跑完一个 crate 的测试后临时根下不留新目录（可以只覆盖上面几个大户）。

## 待决问题

- 前置检查失败时 arc 怎么归类：今天 gate 失败一律走 `command_failed` → 返工，会消耗 auto_rework；环境问题是否应该是不扣返工次数的一类（例如 `retry_wait`）？
- 是否由 gate 自动清理超过一定时间的 `radian-*` 临时目录。

## 验收

- 在 Linux gate 机上连跑 3 次完整 Verify，`/tmp` 下新增的顶层目录为 0（或只剩明确列出的例外）。
- inode 不足时 gate 给出一行明确原因，而不是几百个测试失败。

# arc gate 合批：几个 task 一起过一次完整 Verify

- 状态：decided（可以派发；排在已落地的合并方式之后：合批是合并方式的一种，`method = "batch"`，两项改同一块 integration 代码）
- 涉及 ADR：0048（§10 gate 与 integration 候选、§11 批准与 `verdict_set_digest`、§13 调度循环）
- 来源：arc 第二个 plan 的复盘后的讨论

## 背景

今天每个 task 的完整 Verify 跑两次：`commands` 阶段一次（task 分支叠在 main 上），合并时 `integration` 再一次（叠在当时的 main 上）。完整 Verify 是 gate 里最贵的一步（远端数分钟），而且只有一台机器、一把锁，几个 task 的 gate 只能排队。

merge queue（bors、GitHub merge queue）的做法是合批：把几个已经通过单独检查的改动按顺序叠到 main 上，只对最顶上的结果跑一次完整验证；通过就一起合，失败再拆开找原因。

## 决定（已定）

1. **单个 task 的 gate 只做快速检查**：fmt、clippy、改到的 crate（本机和 Linux 各一次）、review；不跑完整 Verify，通过即"可以合并"。完整 Verify 只在合批的 integration 里跑。
2. **组批时机**：每一轮把当时所有已批准合并的 task（人点了 merge，或 auto 为 `merge` 且 gate 已过）放进一批，不设等待窗口，不设最小批量。批里只有一个 task 时就是今天的单个合并。
3. **叠加顺序**：按 plan 优先级、plan 编号、plan 内 task 顺序与 `after` 依赖叠到 main 上（`main → 合入 A → 合入 B → …`），只对顶上的提交跑一次 `key_commands`；通过则 main 快进到顶上的提交，批里每个 task 都 `done.merged`，历史与逐个合并相同。叠的时候冲突的 task 直接进 `rework_pending`（原因写明与谁冲突），其余照常组批。
4. **批失败的定位**：逐个回退（一批通常只有两三个 task，二分省不了多少）。失败的批最多拆一次：拆开后各自单独验证，仍失败的 task 进 `rework_pending`，原因写明它在哪一批失败、单独验证的结果；通过的照常合并。
5. **中间的合并提交**（批里非顶部的那些）不单独验证；main 只保证每一批的顶部提交是验过的。
6. **记录**：每个 task 的 integration verdict 加 `batch_id` 与 `batch_top_sha`，写明它在哪一批、哪个顶部提交上验证；`verdict_set_digest` 的算法不变（integration 本来就不在 digest 里）。

## 验收

- 三个 task 组成一批：完整 Verify 只跑一次，三个都 `done.merged`，main 历史为三个依次的合并提交。
- 批里一个 task 引入回归：批失败后定位到该 task，它进返工，另外两个合并。
- 叠加冲突的 task 被剔出本批并进返工，原因写明冲突对象。

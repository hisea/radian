# arc gate 合批：几个 task 一起过一次完整 Verify

- 状态：idea
- 涉及 ADR：0048（§10 gate 与 integration 候选、§11 批准与 `verdict_set_digest`）
- 来源：arc 第二个 plan 的复盘后的讨论

## 背景

今天每个 task 的完整 Verify 跑两次：`commands` 阶段一次（task 分支叠在 main 上），合并时 `integration` 再一次（叠在当时的 main 上）。完整 Verify 是 gate 里最贵的一步（远端数分钟），而且只有一台机器、一把锁，几个 task 的 gate 只能排队。

merge queue（bors、GitHub merge queue）的做法是合批：把几个已经通过单独检查的改动按顺序叠到 main 上，只对最顶上的结果跑一次完整验证；通过就一起合，失败再拆开找原因。

## 想法

1. 单个 task 的 gate 只做快速检查（fmt、clippy、改到的 crate、review），不跑完整 Verify；通过即"可以合并"。
2. integration 改为合批：把当时所有可以合并的 task 按 plan 顺序与 `after` 依赖叠到 main 上（`main → 合入 A → 合入 B → …`），只对顶上的提交跑一次完整 Verify；通过则 main 快进到顶上的提交，批里每个 task 都 `done.merged`，历史与逐个合并相同。
3. 叠的时候冲突的那个 task 直接进 `rework_pending`（原因写明与谁冲突），其余照常组批。
4. 批失败时拆开定位：先逐个回退验证，以后可改为二分。

## 待决问题

- 单个 task 不再跑完整 Verify 是否接受（风险：一个 task 的回归要到合批时才发现，并拖住同批其他 task）。
- 组批时机：每轮把当时可合并的全部放进一批，还是设最小批量或等待窗口。
- 批失败的定位策略：逐个回退还是二分；失败的批最多拆几次。
- 中间的合并提交（批里非顶部的那些）没有单独验证过：是否接受（merge queue 普遍接受）。
- `verdict_set_digest` 与 integration verdict 如何表达"这个 task 是在哪一批、哪个顶部提交上验证的"。

## 验收

- 三个 task 组成一批：完整 Verify 只跑一次，三个都 `done.merged`，main 历史为三个依次的合并提交。
- 批里一个 task 引入回归：批失败后定位到该 task，它进返工，另外两个合并。
- 叠加冲突的 task 被剔出本批并进返工，原因写明冲突对象。

# 评测：journal 指标提取器 + 固定任务集

- 状态：idea
- 涉及 ADR：无
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 1 条；分报告 6 §7（注：没有独立的 `6-evals-observability.md` 文件，设计在总报告 README 引用的分报告里）

## 背景

Radian 完全没有评测，改进收益量不出来。journal 已有时间戳、工具起止、每次调用的 usage 和费用，可直接算指标。

## 待决方向

写 journal 指标提取器（成功率、耗时、token、缓存命中率、工具调用数、单次成功成本），搭最小评测集：15 道 Terminal-Bench 2.0 子集题 + 5 道 dogfood 提炼的真实任务，Radian 和 pi 各跑 3 次。Terminal-Bench 适配器正在 Linux 盒子上做（分支 `remote/tbench-lxc`，不在 `main`）。

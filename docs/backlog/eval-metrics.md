# 评测：journal 指标提取器 + 固定任务集

- 状态：idea
- 涉及 ADR：无
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 1 条；分报告 6 §7（注：没有独立的 `6-evals-observability.md` 文件，设计在总报告 README 引用的分报告里）

## 背景

2026-10-20：这一项是 `compaction-threshold.md` 和 `context-pruning.md` 的前提，两项的收益都是每次调用的延迟，只能在固定任务集上改前改后比。指标至少要有：每次调用延迟 p50/p90（按 prompt 大小分桶）、任务总墙钟、压缩次数、压缩后前 5 轮重复 `read` 次数、缓存命中率、成功率。本机 journal 已能算出前几项（见 `context-pruning.md` 与 `compaction-threshold.md` 的数据）。

Radian 完全没有评测，改进收益量不出来。journal 已有时间戳、工具起止、每次调用的 usage 和费用，可直接算指标。

## 待决方向

写 journal 指标提取器（成功率、耗时、token、缓存命中率、工具调用数、单次成功成本），搭最小评测集：15 道 Terminal-Bench 2.0 子集题 + 5 道 dogfood 提炼的真实任务，Radian 和 pi 各跑 3 次。Terminal-Bench 适配器已在 `main`（`benchmarks/terminal-bench/`，Harbor 0.24.0，静态 musl 二进制）；2026-10-20 的 4 题 smoke 在 LXC 上跑通（2 题得分，2 题超时），适配器已能把 token 与费用回报给 Harbor、给 `auth.json` chown，并在传了 `--ak timeout_sec` 时自动给 radian 提前 60 s 的 `--turn-timeout-secs`（2026-10-20 落地）。第一步：15–20 题子集 × 3 次，与 pi 用同一模型对比。

# 基准测试移到 operad

- 状态：decided
- 涉及 ADR：0044
- 来源：2026-11-21 精简调查

## 背景

`benchmarks/`（约 744 KB：terminal-bench 的 Harbor agent 与 `metrics.py`、`repo-tasks`、`journal_metrics.py`、性能基线）是评测和 A/B 的工具，不进 radian 二进制。编排和评测现在由 operad 负责。Verify 里有一步 `python3 benchmarks/terminal-bench/metrics.py --self-test`。

## 决定（已定）

- 把 `benchmarks/` 整个移到 operad 仓库（保留历史的方式由派发时定），radian 仓库里删掉。
- Verify 去掉 `metrics.py --self-test` 这一步：`AGENTS.md` 的 Verify 块、`scripts/verify.sh` 的 `run_step` 行同时改（docs 漂移测试比对两者）。
- radian 里引用 `benchmarks/` 的地方（`run_metrics` 的字段定义、backlog 条目如 `compaction-threshold.md`、性能相关文档）改成指向 operad，或者随 `exec-run-metrics-totals.md` 一起去掉引用。
- `radian-exec` 里偏基准性质的性能测试（`perf_agent`、`perf_long_resume`）若只服务于基准，随之移走；若守护回归，留下并去掉对 `benchmarks/` 的依赖。

## 验收

radian 仓库里没有 `benchmarks/` 目录，也没有指向它的路径；Verify 少一步且全绿；operad 里能跑原来的 terminal-bench 自测和 journal 指标提取。

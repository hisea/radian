# `run_metrics` 只留总数

- 状态：decided
- 涉及 ADR：0044、0049、0007、0010
- 来源：2026-11-21 精简调查

## 背景

`radian exec` 的 `Result.run_metrics`（`radian-exec` 的 `metrics.rs`，约 92 KB）和 ACP 的 `_meta.radian.metrics` 带一整套基准分析字段：`request_log`（最多 500 条）、按模型分、按工具分、压缩耗时、`max_tokens_responses` 等，定义对齐 `benchmarks/journal_metrics.py`。operad 只读 `Result` 的 `usage` 和 `cost`；基准分析读的是 journal 本身，不读 `run_metrics`。

## 决定（已定）

- `run_metrics` 只保留总数：`requests`、`compaction_requests`、`input_uncached`、`cache_read`、`cache_write`、`output`、`reasoning`、`fee_micros`、`fee_incomplete`、`peak_context`、`context_window`、`compactions`、`model_errors`、`retries`。
- 删掉 `tools`、`models`、`request_log`、`request_log_truncated`、`compaction_ms`、`max_tokens_responses`，以及只为它们存在的折叠代码和测试。需要细项的分析从 journal 里算。
- 字段定义不再引用 `benchmarks/journal_metrics.py`，改为在 ADR 0044 里自己定义；"没有记录报告的计数是 `null` 而不是 0"（ADR 0007 J1）不变。
- ACP 的 `_meta.radian.metrics` 跟着收窄（ADR 0049）。
- `docs/user/cli.md`、`acp.md` 同一分支更新。

## 验收

`radian exec --output jsonl` 的 `Result.run_metrics` 只含上面的字段；operad 的 radian harness 不改也能通过；JSONL schema fixture 和 docs 漂移测试同步。

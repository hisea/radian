# `agent-provider-openai-compatible` mutation record

## 网络异常矩阵（resilience，2026-10-16）

一次性 worktree `/tmp/resil-mut`（从 `20a8ac7` 创建，用完删除），`--no-fail-fast`，每个变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试（`tests/resilience.rs`） |
|---|---|---|---|
| M1 | `mask` 的 scrub 改成恒等（错误里带上 key） | 抓住 (rc=101) | `a_stream_error_that_echoes_the_key_is_masked` |
| M2 | `run` 去掉 `idle_timeout` 的 `tokio::time::timeout` 包裹 | 抓住（看门狗触发） | `a_silent_server_hits_the_idle_timeout` |
| M3 | `complete` 去掉取消 `select!` 分支 | 抓住（看门狗触发） | `cancelling_returns_instead_of_hanging` |
| M4 | `finish` 把 `(None, false)` 当作正常结束 | 抓住 (rc=101) | `an_early_eof_after_deltas_is_a_transport_error` |

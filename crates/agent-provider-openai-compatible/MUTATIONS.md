# `agent-provider-openai-compatible` mutation record

## 网络异常矩阵（resilience，2026-10-16）

一次性 worktree `/tmp/resil-mut`（从 `20a8ac7` 创建，用完删除），`--no-fail-fast`，每个变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试（`tests/resilience.rs`） |
|---|---|---|---|
| M1 | `mask` 的 scrub 改成恒等（错误里带上 key） | 抓住 (rc=101) | `a_stream_error_that_echoes_the_key_is_masked` |
| M2 | `run` 去掉 `idle_timeout` 的 `tokio::time::timeout` 包裹 | 抓住（看门狗触发） | `a_silent_server_hits_the_idle_timeout` |
| M3 | `complete` 去掉取消 `select!` 分支 | 抓住（看门狗触发） | `cancelling_returns_instead_of_hanging` |
| M4 | `finish` 把 `(None, false)` 当作正常结束 | 抓住 (rc=101) | `an_early_eof_after_deltas_is_a_transport_error` |

## ADR 0018 超长错误分类（overflow，2026-10-13）

一次性 worktree `/tmp/radian-overflow-mut`（`git worktree add --detach HEAD`，HEAD = `cc71ab4` /
`e8a016a`，**独立 `CARGO_TARGET_DIR`**，用完删除），`--no-fail-fast`；每个变异后 `git checkout --`、
`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M5 | 状态门放进 429（`400\|413` → `400\|413\|429`） | 抓住 (rc=101) | `overflow::tests::only_400_and_413_are_considered` |
| M6 | 完全去掉状态门（任何状态都能分类） | 抓住 (rc=101)；契约层也抓住 | 单元测试同上；`contract_11_context_overflow`（`HTTP 500 context_length_exceeded` 被判为超长） |
| M7 | 把被排除的 `too many tokens` 收进消息表 | 抓住 (rc=101)；契约层也抓住 | `overflow::tests::the_generic_phrasings_are_not_overflow`；`contract_11_context_overflow`（`HTTP 400 "too many tokens"`，加固后） |
| M8 | `mask()` 对 `ContextOverflow` 不 scrub | 抓住 (rc=101) | `a_context_overflow_body_that_echoes_the_key_is_masked` |

M7 起初只有单元测试抓住：契约的反例是 `429 "too many tokens"`，被状态门短路了。于是给契约
补了 `400 "too many tokens"` 反例（`e8a016a`），契约层也能抓住同一个变异。

## ADR 0014 reasoning replay (2026-10-15)

One-shot detached worktree `.radian-worktrees/rr-mutations` (from the branch HEAD, deleted
after); each mutation restored with `git checkout --`, `git status --short` empty at the end.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RR-A | drop the replay entirely (`if let Some(field) = replay` becomes a no-op) | caught (rc=101) | `request::tests::reasoning_is_replayed_only_when_the_model_asks_for_it` |
| RR-B | keep the replay but skip it for an empty reasoning (drop Q3's `""`) | caught (rc=101) | `request::tests::reasoning_is_replayed_only_when_the_model_asks_for_it` (the `messages[2]` empty-string assertion) |
| RR-C | replay even when the config is `None` | caught (rc=101) | `request::tests::reasoning_is_not_sent_to_the_provider` (R4) |

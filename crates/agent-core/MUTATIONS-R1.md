# 变异记录：R1 推理内容（`reasoning`）

日期：2026-10-12。全部变异在一次性检出 `/tmp/reasoning-mut`（`git worktree add --detach HEAD`，HEAD = `1efe6a2`，即 R1 代码全部提交之后）里做：变异 → 只跑对应测试（`cargo test ... -- --exact --no-fail-fast`）→ 恢复文件。跑完 `git status --short` 为空，worktree 已删除。

| 变异 | 目标测试（AC） | 结果 |
|---|---|---|
| M-R1 取消路径丢掉推理（只提交可见文本） | `cancelling_during_reasoning_only_commits_the_reasoning_prefix`（AC-R1-2） | 抓住（rc=101） |
| M-R2 推理混进 `content` | `cancelling_mid_stream_keeps_text_and_reasoning_separate`（AC-R1-3） | 抓住 |
| M-R3 可见文本混进 `reasoning` | 同上（AC-R1-3） | 抓住 |
| M-R4 「空」只看可见文本（只有推理时不提交） | `cancelling_during_reasoning_only_commits_the_reasoning_prefix`（AC-R1-2） | 抓住 |
| M-R5 推理累计器跨调用共享、不重置 | `each_model_call_accumulates_reasoning_from_scratch`（AC-R1-5） | 抓住 |
| M-R6a OpenAI 请求体把 `reasoning` 发出去 | `reasoning_is_not_sent_to_the_provider`（AC-R1-6，OpenAI） | 抓住 |
| M-R6b Anthropic 请求体把 `reasoning` 发出去 | `reasoning_is_not_sent_to_the_provider`（AC-R1-6，Anthropic） | 抓住 |
| M-R7 `reasoning` 为空也序列化（去掉 `skip_serializing_if`） | `reasoning_is_absent_when_empty_and_present_when_set`（AC-R1-7） | 抓住 |
| M-R8 adapter 不读 `delta.reasoning_content` | `reasoning_content_streams_as_reasoning_not_text`（AC-R1-8） | 抓住 |
| M-R9 推理在完成路径上另写一条 journal 记录 | `reasoning_deltas_write_no_journal_records_of_their_own`（AC-R1-9） | 抓住 |

结论：**10/10 全抓，无未抓住项。**

两处变异的写法说明（供复核）：

- **M-R5**：`StreamTap` 是每次模型调用新建的，所以「跨调用泄漏」不能用「`take()` 不清空」表达。变异把推理缓冲改成进程级 `static Mutex`（`emit` 写入它、`take` 读取且不清空），于是第一次调用的推理泄漏进第二次调用的前缀——AC-R1-5 因此失败。这是「两个累计器不分别重置」的忠实等价物，不是惰性变异。
- **M-R9**：`ModelReasoningDelta` 是同步事件出口，本身无法写 journal；「给推理写了 journal 记录」的可执行等价物是在**完成路径**上为推理额外 `commit` 一条 `Message`（即让推理成为独立的 durable 记录）。AC-R1-9 断言完成一轮后 journal 恰好只有 user + assistant 两条 `Message`，因此失败。

`journal-selfcheck`（AC-R1-9 的另一半）由 7 条 Verify 中的 `cargo test -p agent-core --locked --features journal-selfcheck` 覆盖，未单独变异（它断言的是 journal 回放与内存 transcript 相等，M-R1/M-R9 等都会连带触发）。

## 修订 1（2026-10-15）：按模型回传推理

一次性 detached worktree `.radian-worktrees/rr-mutations`（从分支 HEAD 创建，用完删除）；每个变异后 `git checkout --`，结束时 `git status --short` 为空。

| 变异 | 目标测试 | 结果 |
|---|---|---|
| RR-E `count_reasoning` 不计推理（无视开关，永远减去 `reasoning`） | `reasoning_is_counted_only_when_the_budget_asks_for_it`（Q4） | 抓住（rc=101） |

RR-A/B/C（适配器请求体）、RR-D/F（目录解析与 `[providers.<id>]` 覆盖）分别记在 `crates/agent-provider-openai-compatible/MUTATIONS.md` 与 `crates/radian-host/MUTATIONS.md`。

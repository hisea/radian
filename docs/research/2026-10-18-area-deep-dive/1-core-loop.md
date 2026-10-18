# 深挖 1：Core loop（agent 主循环）

调研日期：2026-10-18。Radian 在 `main 356df1e`。对手源码在 scratchpad 的 `comp/` 下（下文简称 `comp/`）：Codex（`codex-rs`），opencode（`63c1a27`，循环代码已从 `packages/opencode` 移到 `packages/core/src/session/runner/`），pi-mono（coding-agent 1.3.0），omp 18.12.1（`@oh-my-pi/pi-agent-core`、`pi-coding-agent`），Claude Code 2.1.302（闭源，只用文档和 `sdk-tools.d.ts`）。

## 1. 一句话结论

Radian 的循环在**持久化和恢复**上比五家都严格。每步宿主开销在组提交（ADR 0040）之后只剩 16.7/26.1 ms，已经不是瓶颈。它慢在别处：**循环不会自己从"无效结束"里恢复**。空回复、`max_tokens` 截断、同一调用反复重试、运行中的长工具挡住 steer，都会白白浪费一轮人工输入或几分钟墙钟。优先补这些便宜的恢复机制。"边流边派发工具"等先有测量再决定：按 Radian 自己的数据，它能省下的时间很小。

## 2. 对比表

| 维度 | Radian | pi | Codex | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 循环结构与终止 | `step()` 循环。没有工具调用且 `EndTurn` 时结束，有 steer 待处理时继续（`turn.rs:1817`） | 两层 while：内层跑工具和 steer，外层接 follow-up（`agent-loop.ts:179-326`） | `needs_follow_up` 由工具调用或待处理输入决定（`session/turn.rs:604`） | 一直跑到某次回复不含工具调用为止（文档 agent-loop） | 同 pi，另有 `turn-recovery` 多种恢复 | `while(true)` 跑 step，`needsContinuation` 决定是否继续（`runner/llm.ts:191-198`） |
| 工具派发时机 | 整个回复收完、assistant 消息入 journal 之后才派发（S4，`turn.rs:1800`） | 整个回复之后（`agent-loop.ts:263-270`） | **边流边派发**：每个 `OutputItemDone` 的调用立刻放进 `in_flight`（`stream_events_utils.rs:315-358`，`turn.rs:2908`） | 未公开 | 默认整个回复之后；**推测执行**默认关，只用于 pure/local_read 调用（`tools/settings.ts:839-857`） | **边流边派发**：收到 `tool-call` 事件就 fork（`runner/step.ts:118-133`） |
| 并发分类 | `Shared`/`Exclusive`，按 ToolSpec 固定。shell 是 Exclusive，MCP 看 `readOnlyHint` | 默认 `parallel`；只要有一个 `sequential` 工具，整批串行（`agent-loop.ts:516-523`） | RwLock：支持并行的拿读锁，其余拿写锁，默认不支持（`tools/parallel.rs:213-216`，`tool_executor.rs:122`） | 只读工具并发，Edit/Write/Bash 串行；自定义工具默认串行，`readOnlyHint` 开启并行（文档） | 默认 `shared`，可以按参数决定：非 pty 的 bash 是 shared（`bash.ts:632`），edit/write 是 exclusive | 全部并发，写文件靠按路径加锁（`file-mutation.ts:60-75`） |
| 并发上限 | 4（`turn.rs:31`） | 不限 | 不限（读锁） | 未公开 | 不限 | 不限 |
| steer 进入时机 | 每步开头，也就是下一次模型调用之前（`turn.rs:1516`）。默认 `AfterBatch`，宿主从不设置 `SkipRemaining` | 每次助手回复之后；默认一次取一条（`agent.ts:247`） | 采样之间注入；收到 reasoning/commentary 项时可以**抢占**正在流的回复（`turn.rs:2915-2933`） | 流式输入；回合因 max-turns 结束时，排队消息另起新回合（文档） | 批次边界注入；给运行中工具发**软信号**，可后台化的 bash 自己转后台（`agent-loop.ts:3292-3310`） | inbox 的 steer/queue 两种投递（`runner/llm.ts:60-180`） |
| 运行中工具与 steer | 不打断。前台 shell 最多挡 120 s 后自动转后台；`job_output wait` 最多挡 600 s | 不打断 | 不打断（未见软信号） | 未公开 | 纯等待类工具直接打断；其他工具只收软信号 | 不打断 |
| 暂时性错误重试 | 3 次，2/4/8 s 加抖动，合计 ≤60 s，尊重 Retry-After（`turn.rs:162-183`） | 3 次，基础 2 s（`settings-defaults.ts:20-22`） | 流式 5 次、请求 4 次，**200 ms** 起步翻倍（`backoff.rs:7-17`，`model-provider-info/src/lib.rs:66-68`），还能切换传输方式 | 未公开 | 3 次，另有 stall/premature-close 的专门分类（`turn-recovery.ts:92-120`） | 10 次，2 s 起，单次间隔上限 10 s，约 84 s；超时类最多 3 次（`model-retry.ts:23-40`） |
| 流在中途断开 | 丢弃部分输出，整次重来（ADR 0017） | 整次重来 | 整次重来（未见续写） | 未公开 | 已解决的工具回合上保留并续写（`turn-recovery.ts:108-114` 注释） | **保留已输出的部分**，再注入"从断处继续"（`runner/llm.ts:35`、`:281-288`） |
| `max_tokens` 截断 | 有工具调用：返回 `truncated` 结果并继续（`turn.rs:1907`）；没有工具调用：**结束回合**，状态 Incomplete（`turn.rs:1829`） | 工具调用全部判失败，然后继续（`agent-loop.ts:478-502`） | 未核实 | 未公开 | 未核实 | 注入"拆小再做"，**最多续写 2 次**（`runner/llm.ts:38`、`:290-318`） |
| 步数上限 | core 默认 32，宿主默认 1000（`agent.rs:17`，`radian-host/src/model.rs:36`）。用完直接 `BudgetExceeded` | 无 | 未发现 | `maxTurns` 默认不限，另有 `maxBudgetUsd`（文档） | 未核实 | 按 agent 配置。最后一步**保留工具定义但 `toolChoice:none`**，并强制要求总结（`runner/llm.ts:226-244`，`max-steps.ts`） |
| 空回复/意外停止 | 视为 `Completed` | 无 | 无 | 未公开 | 默认 `mechanical`：空停止自动重试，最多 3 次（`settings.ts:1052-1056`，`turn-recovery.ts:98`） | 未核实 |
| doom-loop | 无 | 无 | 无（grep 未找到） | 未公开 | 连续 5 次参数相同的调用注入纠正提示（`settings.ts:311-330`）；还能检测推理和正文里的流式重复 | 当前 core 没有触发点，只剩 v1 配置键 `doom_loop`（`v1/config/permission.ts:27`） |
| 推理回放 | 按模型：OpenAI 兼容用 `reasoning_replay` 字段，Responses 用 `encrypted_content`，Anthropic 用 `provider_payload`（ADR 0014） | 按 provider 原样写回 | Responses 原生 | 原生 | 有 `replay-policy.ts` | 未核实 |
| 计划/todo | 无 | 无（设计如此） | `update_plan`（`plan_spec.rs:43`） | `TodoWrite`、`TaskCreate`/`TaskUpdate`（`sdk-tools.d.ts:1098,2802`） | `todo` 工具（exclusive） | 当前 core 未见 |
| 长工具期间保温 prompt cache | 无 | 默认 `streaming`：在 TTL 的 90% 时刷新（`cache-warmer.ts:15-31`，`settings-defaults.ts:61`） | 无（有启动 prewarm，`startup_prewarm.rs`） | 未公开 | 未核实 | 无 |
| 每步可观测 | 只有 journal 的 ts。流的 delta 只在 live 里，不入 journal（ADR 0010） | 事件流 | 每回合记录 TTFT、TTFM、采样/工具/压缩各段耗时（`turn_timing.rs:19-195`） | 未公开 | telemetry span | 事件总线 |

## 3. 各家关键做法（只列和"快、有效率"有关的）

**Codex**
- 边流边执行：`handle_output_item_done` 收到一个完整的工具调用项，先把它持久化（`record_completed_response_item`），再把 future 放进 `FuturesOrdered`。流结束后才 `drain_in_flight`（`codex-rs/core/src/stream_events_utils.rs:324-358`、`session/turn.rs:3283-3291`）。顺序是"先记录、再派发"，和 Radian 的 E3 精神一致，只是粒度从整条消息变成了单个调用。
- 并发靠一把 `RwLock<()>`。`supports_parallel_tool_calls` 默认 `false`，shell、apply_patch 都拿写锁（`tools/parallel.rs:141-216`）。也就是说 Codex 的 shell 和 Radian 一样串行。
- 重试起步快：200 ms 起翻倍，±10% 抖动（`async-utils/src/backoff.rs:7-17`），重试用完还能切换回退传输（`responses_retry.rs:120-137`）。
- 邮箱抢占：模型还在流 reasoning/commentary 时，如果用户发了消息，就提前结束这次采样，带上新输入重新请求（`session/turn.rs:2884-2933`）。
- 异步压缩：回合中途在后台预先算好压缩候选，到达阈值时直接采用（`session/async_compaction.rs:1-2`，`turn.rs:606-694`）。这属于压缩区域，这里只记一笔。
- `turn_timing.rs` 记录每回合的 TTFT/TTFM 和采样、工具阻塞、压缩各段，用来定位慢在哪里。

**opencode**（`packages/core/src/session/runner/`）
- 收到 `tool-call` 流事件就 `forkScoped` 执行，不分只读或写（`step.ts:118-133`）。并发写由按路径的 `KeyedMutex` 串行化（`file-mutation.ts:60-75`）。
- 起始快照和 provider 请求并行，"captured while the provider request is in flight instead of delaying it"（`step.ts:74-76`）。标题生成也不挡主循环（`llm.ts:54`）。
- 流在输出开始后才断：保留已输出部分，注入 `CONTINUE_AFTER_INCOMPLETE_STREAM`，然后续写（`step.ts:196, 263-271`；`llm.ts:281-288`）。
- `finish=length`：注入"不要道歉、不要复述，把剩下的拆小"。最近 3 条消息都是 length 时判失败，所以最多续写 2 次（`llm.ts:290-318`）。
- 步数到顶时，最后一步仍然带工具定义（"preserve the provider's cached prefix"），设 `toolChoice:none`，并追加强制总结的提示（`llm.ts:226-244`，`max-steps.ts:1-16`）。这样用户拿到的是总结，不是一个中断。

**pi**
- 循环极简：调用一次模型，执行工具，再取 steer，再取 follow-up（`packages/agent/src/agent-loop.ts:163-326`）。工具默认并行；只要批里有一个 `sequential`，整批退成串行（`:508-523`）。
- 工具结果可以带 `terminate: true`：整批都要求终止时，就省掉最后那次模型调用（`:690-692`）。
- `stopReason === "length"` 时，所有工具调用一律判失败，提示模型重发（`:478-502`），和 Radian 相同。
- **prompt cache 保温默认开**（`streaming`）：一次真实请求之后，到 TTL 的 90%（至少留 10 s 余量）时发一次重放请求来续缓存；预计省不到 0.05 美元时不发（`coding-agent/src/core/cache-warmer.ts:15-31`，`settings-defaults.ts:61`）。

**omp**
- 调度和 Radian 一样分 shared/exclusive，但**默认是 shared**，而且可以按参数决定：非 pty 的 bash 是 shared，pty 是 exclusive（`pi-coding-agent/src/tools/bash.ts:629-633`；`pi-agent-core/src/agent-loop.ts:3834-3858`）。
- steer 和工具的配合：给运行中的工具一个"软"`steeringSignal`。可以自动后台化的 bash 收到后就转后台，消息于是尽快注入；纯等待类工具直接打断；有副作用的工具从不被硬杀（`agent-loop.ts:3292-3310`，轮询间隔 250 ms，`:197`）。
- Responses 的 live steering：把排队的 steer 塞进 provider 正在流的那次回复（`live-steering.ts:1-30`）。
- 恢复层（`pi-coding-agent/src/session/turn-recovery.ts:92-120`）：空停止、意外停止、畸形函数调用、stream stall 各自最多重试 3 次；`smart` 模式还会用小模型判断"是不是提前停了"（默认 `mechanical`，`settings.ts:1052-1056`）。
- 循环保护：连续 5 次参数相同的调用，注入 `<system-interrupt reason="tool_call_loop_detected">`，要求换参数、换工具或者总结（`settings.ts:311-330`，模板 `prompts/system/tool-call-loop-redirect.md`）。另一层检测推理和正文里的流式重复（`model.loopGuard.*`，默认开）。
- 推测执行（默认关，`maxInFlight=2`）：只用于声明为 `pure`/`local_read` 的调用，结果在正式派发时才提交，读到的文件变了就作废（`speculative-execution.ts`，`types.ts:684-697`）。

**Claude Code**（只依据文档 https://code.claude.com/docs/en/agent-sdk/agent-loop）
- 只读工具并发，"Tools that modify state (like Edit, Write, and Bash) run sequentially"；自定义工具默认串行，靠 `readOnlyHint` 开启并行。Radian 的 MCP 分类与此一致。
- `maxTurns` 只计工具往返，默认不限；另有 `maxBudgetUsd`，在每次响应之后检查。
- 有 `TodoWrite`/`TaskCreate` 一类计划工具（`sdk-tools.d.ts`）；`Stop` hook 用于"Validate the result"。
- 是否边流边执行、重试参数：**未公开**，本文不推断。

## 4. Radian 的优势

1. **崩溃语义最严**：调用的副作用发生之前，`ToolExecutionStarted` 一定已经落盘（ADR 0002 E3，ADR 0040）。恢复时能区分"没开始"和"结果未知"，别家没有这样的纯函数恢复规则。
2. **固定开销已经很低**：进程内一轮 16.7 ms，一次工具往返 26.1 ms（release，ADR 0040 之后，`docs/perf-baseline.md` §2）。真实会话里模型时间占 24.6%，工具时间占 65.5%（`2026-10-15-tool-concurrency-measurement.md` §5），宿主开销可以忽略。
3. **重试有上限、不白等**：合计 ≤60 s，尊重 Retry-After；`usage_limit_reached` 不重试（ADR 0017 补记：此前实测白等了 58 分钟）。opencode 的 Retry-After 上限是 15 分钟，一次调用可能挂很久。
4. **工具结果按调用顺序提交**，并发不改变 transcript 的顺序（`Batch::commit_ready`），对 prompt cache 友好。
5. **`max_tokens` + 工具调用**的处理和 pi 相同，而且更细：`content_filtered` 不执行，`PauseTurn` 继续。
6. **steer 在最终回答期间到达时，回合继续**（`turn.rs:1817-1825`），不会丢掉跟进消息；回合有 `deadline`，取消有宽限期（`TurnOptions`）。
7. 上下文溢出时，按"压一半、重试一次"兜底（ADR 0018），不会死循环。
8. 推理按模型回放（ADR 0014），避免 DeepSeek 类 400 让会话卡死（pi 曾卡在同一处）。

## 5. Radian 的劣势和缺口

1. **没有空回复/意外停止恢复**：`EndTurn`、没有文本也没有工具调用，直接 `Completed`（`turn.rs:1817-1828`）。用户只能再打一句"继续"，浪费一轮人工输入。omp 默认会自动处理。
2. **`max_tokens` 且没有工具调用时，结束回合**（`IncompleteReason::MaxTokens`，`turn.rs:1829`）。长回答或长 patch 写到一半就停，需要人来续。opencode 会自动续写 2 次。
3. **没有 doom-loop 检测**：同一个失败调用可以一直重复，直到 `max_model_calls`（宿主默认 1000）。只有 omp 有这层保护。
4. **steer 被长工具挡住**：`SteerMode` 默认 `AfterBatch`，宿主从不设置 `SkipRemaining`；`ToolContext` 里也没有"有 steer 待处理"的信号。前台 shell 最多挡 120 s（`radian-coding/src/provider.rs:88`）才转后台，`job_output wait` 最多挡 600 s（`jobs.rs:69-70`）。在实测会话 ab3 里，`job_output` 的中位耗时是 361 s（测量文档 §3）。这段时间里用户的纠正消息一直进不去。
5. **网络抖动时第一次重试慢**：所有暂时性错误一律 2 s 起步（`turn.rs:162`），连接重置也一样；Codex 是 200 ms。
6. **流在中途断开就整次重来**：已流出的长输出全部丢弃，再付一遍 TTFT 和输出 token。opencode 会续写。
7. **长工具让 prompt cache 过期**：实测 shell 的 p90 是 72 s，最长 600 s，`job_output` 的中位是 361 s（测量文档 §3）。都超过了 Anthropic 默认 5 分钟的 TTL（推断：要看 provider 配置），下一次请求就要按写缓存计价，TTFT 也更长。pi 默认会保温。
8. **测不到"边流边派发"能省多少**：流的 delta 只在 live 里（ADR 0010），journal 没有 TTFT、流时长，也没有工具调用块完成的时刻。测量文档明确说这一项"不可测"。
9. 次要：每次模型调用都克隆整个 transcript（`turn.rs:1525`），每次尝试还要再 `request.clone()`（`:273`）；预算估算每步把整个请求序列化 1 到 2 次（`budget.rs:156-186`）。量级推断为长会话里每步几毫秒，没有测过。共享组里每个 `Started` 都单独 fsync（`radian-session/src/writer.rs:172-187`），约 4 ms 一次。

## 6. 改进方案（按收益/成本排序）

### P1 空停止的机械恢复（收益高，成本低）
- **做什么**：`StopReason::EndTurn`、可见文本为空、没有工具调用、但本轮已经有过工具结果时，不结束回合，追加一条带 `MessageMeta` 标记的合成 user 提示，例如"你的上一条回复是空的。继续完成任务，或者说明已经完成。"，然后 `Step::Continue`。每轮最多 2 次，用完再按 `Completed` 结束。只有推理、没有正文，也算空。
- **为什么**：省掉一轮人工"继续"。omp 把它做成默认（`turn-recovery.ts:98`，默认 `mechanical`）。
- **成本**：agent-core 的 `step_once` 加几十行，再加一个计数字段；合成消息走现有的 `commit`，也就进了 journal。
- **ADR**：改变了回合的终止条件，需要**新 ADR**（或 ADR 0017）；合成消息的来源标记要符合 ADR 0005（host message metadata）。
- **验证**：用 `ScriptedModel` 返回空的 EndTurn，断言第二次模型调用发生、journal 里有合成消息；第三次仍然为空时结束。变异：去掉上限，测试应当失败（死循环）。跑 `journal-selfcheck`。

### P2 doom-loop 检测，做成宿主 hook（收益高，成本低）
- **做什么**：在 radian-host 写一个 `after_tool_call` hook，记录最近 N 次调用的 `(name, canonical args)`。连续第 5 次相同（阈值沿用 omp）时，在该调用的结果末尾追加一段纠正文字："你已连续 5 次以相同参数调用 X，结果如下……不要再这样调用；换参数、换工具，或总结后结束。"`job_output` 这类轮询工具加入豁免表（omp 也有 `exemptTools`）。
- **为什么**：把"重复到 1000 次上限"变成第 5 次就纠正。改写工具结果而不是另插一条消息，transcript 结构不变，cache 前缀也不变。
- **成本**：不改 agent-core（`ToolDecision`/`after_tool_call` 已有），只在宿主加一个 Hooks 实现；约 150 行加测试。
- **ADR**：新的宿主行为，写一个短 ADR（是否默认开、阈值、豁免表）；不碰 0002，`Settled.result` 存的就是 hook 之后的最终结果（E1）。
- **验证**：脚本模型连续发 6 次相同的 `read`，断言第 5 个结果含纠正文字、第 4 个不含；参数改一个字节，计数归零。变异：比较时忽略参数，测试应当失败。

### P3 `max_tokens` 且没有工具调用时自动续写（收益中高，成本低）
- **做什么**：`StopReason::MaxTokens` 且没有工具调用时，提交截断的消息，注入 opencode 式的提示（"输出到了上限。不要道歉、不要复述，把剩下的拆成小块继续"），然后 `Continue`。连续 2 次之后才按 `Incomplete(MaxTokens)` 结束。
- **为什么**：opencode 用 3 条连续 length 判失败，即最多续写 2 次（`runner/llm.ts:290-318`）。省掉一轮人工续写；"拆小"的提示也减少下一次再被截断的概率。
- **成本**：低，与 P1 共用"合成提示加计数"的机制。
- **ADR**：同 P1，改的是终止条件，放进同一个新 ADR。
- **验证**：脚本模型连续返回 MaxTokens 3 次，断言第 3 次后结束、前两次各注入一条提示；有工具调用的现有路径（`truncated`）不变。

### P4 给 steer 一个"软信号"，让长工具让路（收益高，成本中）
- **做什么**：
  1. `ToolContext` 加一个 `steer_pending: CancellationToken`（或 `watch`）。只要 `SteeringHandle` 里有消息，`run_group` 就触发它。
  2. `shell` 收到信号时，如果还没超过 auto-background 阈值，立即 `adopt` 转后台（复用 ADR 0037 的 adopt 路径），返回 job id。
  3. `job_output wait=true` 收到信号就提前返回当前增量，状态写明"用户发来新消息，等待被中断，任务仍在运行"。
  4. 有副作用的工具一律不硬杀，与 omp 相同。
- **为什么**：steer 的延迟从最多 120 s（前台 shell）或 600 s（`job_output`）降到毫秒级。实测里正是这两类调用占了绝大多数工具时间（测量文档 §3：shell 合计 7746 s，job_output 1991 s）。用户的纠正越早进入，越少浪费在错误方向上的工具时间和后续轮次。omp 已有同样的设计（`agent-loop.ts:3292-3310`）。
- **成本**：中。agent-core 公共 API 新增一个字段（按 AGENTS.md，要写明理由），shell 和 jobs 各接一处，加测试。
- **ADR**：ADR 0011（steer 何时生效）需要修订；ADR 0037（自动后台化的时机）需要修订。ADR 0002 不变，因为转后台的调用照常 `Settled`。
- **验证**：前台 `sleep 30` 期间注入 steer，断言工具在 <1 s 内返回 job id，下一次模型请求里有 steer；`job_output wait` 同理。时序测试用 tokio 暂停时钟，不用 `sleep`。变异：不触发信号，测试应当超时失败。

### P5 传输类错误的第一次重试更快（收益中，成本很低）
- **做什么**：`ModelError::Transport`（连接重置、读超时之类）的第一次重试延迟改为约 250 ms，之后按 2/4 s 走；`Unavailable`（429/503）保持 2 s 起步，并继续尊重 Retry-After；60 s 总上限不变。
- **为什么**：网络抖动不是服务端过载，不需要退避 2 s。Codex 统一从 200 ms 起步（`backoff.rs:7`）。每次抖动省约 2 s，长会话里会累加。
- **成本**：改 `retry_delay` 的一个分支，加测试。
- **ADR**：ADR 0017 写死了"2 s、4 s、8 s"，需要修订 2。
- **验证**：现有重试测试加一个 Transport 用例，用暂停时钟断言等待约 250 ms；429 用例保持 2 s。

### P6 把流的时序写进 durable 用量记录（收益是"能测"，成本低）
- **做什么**：在 `ModelUsageRecorded` 里加可选字段 `ttft_ms`、`stream_ms`，以及每个工具调用块在流中完成的偏移 `tool_call_ready_ms[]`（adapter 在解析时就有这些时刻）。
- **为什么**：现在测不出 P8（边流边派发）的收益（测量文档"What cannot be measured"）。有了这些字段，用现有脚本就能算"最后一个调用就绪到 message_stop 之间的时间"乘以调用数，也能跟踪各 provider 的 TTFT。Codex 有同类计时（`turn_timing.rs`）。
- **成本**：低。adapter 打时间戳，记录加可选字段。
- **ADR**：ADR 0006（JSONL 格式，加可选字段不升版本，按其规则核对）、ADR 0007（usage）、ADR 0010（这些是 durable 事实，不是 live delta）、ADR 0013（只写本地文件，不上报）。
- **验证**：契约测试断言两个 adapter 都填了这些字段，或者按 `docs/provider-contract.md` 声明"不支持"；`check-fixtures.sh` 通过。

### P7 长工具期间保温 prompt cache（收益：长等待后的成本和 TTFT，成本中）
- **做什么**：工具运行超过 cache TTL 的 90%（至少留 10 s 余量）时，用上一次请求的前缀发一次 `max_tokens=1` 的保温请求，用量写进 journal。照搬 pi 的阈值规则：预计省钱不足就不发。
- **为什么**：Radian 的长 shell 和 `job_output` 常常超过 5 分钟（§5 第 7 条）。pi 默认开启（`cache-warmer.ts:15-31`）。
- **成本**：中。需要定时器、provider 的 TTL 信息（目录）、计价，还要处理与取消的交互。和缓存区域重叠，**建议与区域 2/缓存的结论合并后再定**。
- **ADR**：新 ADR；ADR 0007（这类用量要单独标记，不能挪动预算基线，参照 `CompactionUsage` 的做法）。
- **验证**：mock provider 加暂停时钟：工具挂 5 分钟，断言恰好发出一次保温请求，且它的用量不进 `budget_baseline`。

### P8 边流边派发 `Shared` 调用（收益未知，可能小；成本高）
- **做什么**：先做 P6，测出"调用就绪到 message_stop"的分布。只有当收益明显（比如多调用响应的占比上升，或某个 provider 的尾部很长）时，才让 adapter 在一个工具调用块完成时发出 `ToolCallReady`，由 core 写一条单调用的 durable 记录后立即执行 `Shared` 调用；`Exclusive` 调用仍然等整条消息。
- **为什么不急**：实测 93.1% 的响应只有一个调用（416/447）。单调用时，能省的只是工具块结束到 message_stop 之间那一点尾巴（推断：通常几十毫秒）。Codex 和 opencode 这样做，是因为它们的调用多数并行，而且 Codex 先持久化再派发；omp 默认也关着自己的推测执行。
- **成本**：高。`Model` trait 要改（agent-core 公共 API），ADR 0002 的 E3/S4（assistant 消息先入 journal）要重写，恢复规则要处理"消息未完成、调用已开始"，还有 provider 契约测试。
- **ADR**：0002、0003（中断前缀里可能已有已执行的调用）、0010，还需要新 ADR。

### P9 步数到顶时的收尾一步（收益低到中，成本低）
- **做什么**：`model_calls == max_model_calls - 1` 时，最后一次请求保留工具定义（保住 cache 前缀），设 `tool_choice: none`，并追加"已到步数上限，总结已完成的、未完成的和下一步"的提示。
- **为什么**：用户拿到可以接着用的总结，而不是一个 `BudgetExceeded`。opencode 就是这么做的（`runner/llm.ts:226-244`）。宿主默认 1000 步，很少触发；对 `radian exec -n` 小预算和 subagent 更有用。
- **成本**：低。但 `tool_choice` 要在三个 adapter 里都有对应的 wire 值，按 provider 契约不能编造。
- **ADR**：新 ADR，或者并入 P1 的终止条件 ADR。

### P10 共享组只做一次 fsync（收益很小，成本中）
- 一个 Shared 组先依次 `write` 每个 `Started`，派发前只 sync 一次。省下 `(k-1) × ~4 ms`。按实测，多调用组本来就少（31/447），**不建议现在做**；需要修订 ADR 0040 的第 2 条。

### 不建议做的
- **把 shell 改成 Shared**（omp 的做法）：实测在多调用响应里能省的上限只有 0.27 s，占墙钟的 0.0018%（测量文档 §4）；Codex 和 Claude Code 也都让 shell 串行。收益太小，不值得承担并发副作用的风险。
- **opencode 式的全并发加文件锁**：理由同上，而且它会改变 Radian 的 Exclusive 语义（ADR 0002 的恢复规则就是按这种语义写的）。

## 7. 局限

- 没有运行任何对手程序，结论都来自读源码。Claude Code 只用文档，凡标"未公开"的不做推断。
- 测量文档用到的三份 journal（`.radian/data/*.jsonl`）不在本机，所以无法再从中统计空回复、截断、重复调用各出现了多少次。P1–P3 的收益是机制推断，不是实测数字。上线前建议用 P6 的字段，加一个"空停止/截断/重复"计数脚本，在 dogfood 会话上量一次。
- opencode 的源码树是新的 v2 core。旧版 `packages/opencode/src/session` 里的 doom-loop 实现（如有）没有核实。

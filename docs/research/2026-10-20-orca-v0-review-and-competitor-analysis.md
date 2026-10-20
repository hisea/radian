# Radian Orca v0：设计审核与竞品分析

> **审核日期**：2026-10-20  
> **审核对象**：`hisea/radian` 主分支的三份 Orca 文档及相关已接受 ADR  
> **结论**：**保留 Orca 的架构方向，但当前 v0 设计不宜直接实施；先修订状态、恢复、Git 发布及验证契约。**  
> **项目状态**：原 backlog 明确写为 `idea / 不开工`；本报告是审查和建议，**不表示已修改仓库或批准开工**。

## 摘要

Orca 的正确定位是**独立于 Radian Core 的确定性任务编排与交付控制器**：由 Claude（lead）规划、拆解和决定返工；由 Orca 负责唯一调度权、持久化状态、跨主机执行、验证及交付证据；由 Radian worker 在独立 worktree 中完成编码；由 owner 审批和合并。

这个分层方向符合 Radian 的单会话、最小 Core 原则。不过，现有文档存在多处会影响实际正确性的冲突：

1. **MCP 与 `orca run` 的并发写入语义没有定义**。整个 run 周期持有 `flock` 与随时能 `add/cancel` 的 MCP 入口冲突。
2. **SSH worker 的崩溃恢复未闭合**。重启后只看进程 PID，不能证明远端任务已停止，也不能无损接管；可能重复派发和覆盖工作区。
3. **`done` 的定义不一致**。有时表示 Gate 通过，有时表示分支已经合入 main；依赖解锁条件因此不确定。
4. **Git 工作流遗漏远端 commit 的发布步骤**。Orca 在 Mac 收到 LXC worker 的 SHA，不代表 Mac 本地就能 `git push` 那个 ref。
5. **无人值守 worker 默认权限过宽**。ADR 0043 规定 `full` 是 Radian 默认档；原 `radian exec ... --yes` 未显式指定 sandbox。
6. **Gate 默认过重**。将 mutation 设为所有任务 required，不利于验证最小调度闭环；独立 reviewer 也已不是市场空白。
7. **已有命令接口与远程 UI 假设有落差**。`radian attach` 是只读 transcript TUI，不是现成的 JSONL 实时事件 API；`Result.cost` 当前为会话累计。

**建议的最小方向**：保留目录式 backlog、单控制面、多主机 SSH、固定代码 SHA 的 commands gate、human gate。引入 `item_id/session_id/attempt_id`、事务型写入、保守的远端状态对账、合并前 SHA 检查。将 mutation、高级 review、复杂 gate DSL 和 iOS/Web UI 延后。

---

## 1. 审核范围与证据等级

### 1.1 项目原始文档

- [Backlog：`docs/backlog/orca.md`](https://github.com/hisea/radian/blob/main/docs/backlog/orca.md)
- [设计：`docs/research/2026-10-20-orca-v0-design.md`](https://github.com/hisea/radian/blob/main/docs/research/2026-10-20-orca-v0-design.md)
- [竞品调查：`docs/research/2026-10-20-orca-competitors.md`](https://github.com/hisea/radian/blob/main/docs/research/2026-10-20-orca-competitors.md)

### 1.2 对照的 Radian 契约

- [ADR 0043：单会话 Radian](https://github.com/hisea/radian/blob/main/docs/adr/0043-single-session-radian.md)
- [ADR 0044：`exec --output jsonl` 与退出码](https://github.com/hisea/radian/blob/main/docs/adr/0044-exec-jsonl-output.md)
- [ADR 0032：一个二进制](https://github.com/hisea/radian/blob/main/docs/adr/0032-one-binary.md)
- [ADR 0022：session fork 语义](https://github.com/hisea/radian/blob/main/docs/adr/0022-session-fork.md)
- [现有 control port 协议](https://github.com/hisea/radian/blob/main/crates/radian-host/CONTROL.md)
- [已决定但尚列在 backlog 的 Result 本次用量字段](https://github.com/hisea/radian/blob/main/docs/backlog/exec-result-run-usage.md)

### 1.3 方法和限制

- **已确认事实**：以上 Radian 文件内容，以及本报告参考文献中可核查的竞品官方说明、规格和仓库文档。
- **设计判断**：风险严重性、状态机建议、实施顺序、差异化定位均是本次审核意见，不等于代码已经实现。
- **未验证**：未实际运行 Orca 原型、SSH 故障注入、沙箱中 Git worktree commit、跨主机 resume 或一次完整需求的 end-to-end 流程。
- **竞品能力边界**：公开文档声称支持某项功能，不代表已通过独立可靠性测试；未找到说明不等于不具备该能力。

---

## 2. 整体架构评判

### 2.1 应当保留的设计

1. **Radian Core 继续单会话**：Orca 独立处理规划后的队列、worktree、资源、验证、重试、合并；不把 workflow、lead 或工作区池放回 agent-core。这与 ADR 0043 一致。
2. **Lead 和 orchestrator 角色分离**：Claude 决定“做什么、是否返工”，Orca 负责“何时执行、在哪里执行、是否满足明确条件”；模型输出不能直接改调度状态。
3. **每项任务隔离执行**：worker 进程一次性，Git worktree 隔离；Agent 可以写自己的源代码工作区，但不能写 backlog。
4. **Worker 结果结构化**：优先使用 ADR 0044 的 JSONL 和 `Result`，而非屏幕抓取、任意模型文本或与 agent 共享 Markdown 任务状态。
5. **人类仍负责交付决定**：commands、独立 review 给证据，人类作出最终审批/合并决定。
6. **先仓库外原型**：在验证工作流程前不新增 Radian Core 的状态机、工作流抽象或 UI 宿主层。

### 2.2 不应误判的差异点

- **SSH、多主机、worktree、agent adapter、CI 返工**已是若干成熟/活跃竞品拥有的能力，不构成独特壁垒。
- **独立 reviewer**不再是独有想法：OrchestratorInc 官方文档已描述专用 reviewer agent；Sortie 已提供 self-review、CI/review feedback。区别必须具体到 reviewer 的权限、执行隔离及验证证据绑定。
- **非 daemon `run` 排空即退**是产品取舍，而不必然是优势：更容易部署和理解，但不会主动轮询、接收远程推送或保证实时重试。
- **固定 SHA 的独立验证 + 可恢复交付状态**是值得验证的定位，但需要通过真实场景证明比脚本 + CI 更省事、更可靠。

---

## 3. P0：实现前必须解决的设计问题

### P0-1. MCP Server 与 `orca run` 的单写者冲突

**原稿现状**：`orca run` 获取 backlog 独占 `flock`，但 MCP server 也可随时 `add/list/rework/cancel`。如果 MCP 不持锁，违反“唯一写者”；如果长时间持锁，另一入口无法工作。

**建议契约**：

- “单写者”解释为**任一状态变更事务只有一个持锁写入者**，而不是“任一时刻只能有一个 Orca 进程”。
- 对 `add`、`claim`、`record_result`、`transition`、`pass`、`cancel` 采用短事务锁；锁内检查期望版本、修改文件、原子替换、完成耐久化，锁外进行 SSH、Git、Gate 等慢操作。
- 每次状态更新带 `state_version` 或 attempt token 进行 compare-and-swap，防止旧执行结果覆盖新状态。
- 单控制面与命令语义在 v0 保持一致；到 v1 `serve` 可把所有写操作改为统一处理器，但外部 API 不变。

> 注意：仅有 `rename` 原子性并不保证文件内容已经 durable；应明确临时文件写入、文件 `fsync`、同文件系统 `rename` 和必要的父目录 `fsync` 顺序。为 v0 可以缩小故障模型，但必须写在契约中。

### P0-2. SSH 断开与 worker 进程故障不能混为一谈

**原稿现状**：Orca 读 `doing/meta.toml` 的 PID，若进程活着就“接管等待”，否则按失败处理。对于 SSH 子进程，控制端连接中断时远端进程可能仍在运行；PID 会复用，`kill(pid,0)` 也不是完整的任务身份。

**风险**：双 worker 同时编辑同一 worktree、重复 push、审计丢失、旧 attempt 的结果被错误接受。

**建议契约**：

- 为每次执行分配唯一 `attempt_id`，保存 `{item_id, attempt_id, host, remote_process_identity, started_at, session_id, worktree, branch}`。
- 远端结果采用原子落盘的 attempt 结束记录，或经可信 host-side wrapper/supervisor 提供状态查询；不能只相信 ssh 命令的退出状态。
- 连接断开但无法确认旧 attempt 是否停止时，将任务设为 `reconciling` / `orphaned`，**暂停自动重试**。
- 只有旧 attempt 确认退出或被终止后，才能启动下一次；返回的旧 attempt 结果必须因 attempt token 不匹配而拒绝。
- v0 不承诺对未知远端进程“无缝接管”，保守地停下等待人工处理，比错误的自动恢复更安全。

### P0-3. `done` 必须只有一个定义

**冲突**：设计 Gate 说阶段全通过便转 `done`，使用步骤却说 owner 把分支合入 main 后才算 `done`。

**建议**：

- `worker completed` = Agent 正常退出；
- `verified` = 对特定 commit SHA 的必要 Gate 通过；
- `approved` = owner 对该 SHA 做出接受决定；
- `merged` = commit 已以可验证方式被纳入目标分支；
- `done` 默认仅代表 **merged**，否则仍在 `review/`，附 `phase=ready_to_merge`。

依赖关系缺省依赖已合并的成果；否则还需额外设计 stacked branch、基于前驱 worktree 的分支创建和集成冲突处理，不应混进 v0。

### P0-4. `Result.outcome=completed` 不是交付成功

ADR 0044 的 `completed` 表示 Radian 结束了执行链，并不保证任务达成。甚至“工具被拒后模型放弃”的文本也可能被记录为 completed。

Orca 必须在 worker 退出后独立验证：

- 指定 `base_sha` 与实际 `head_sha`；
- 确认预期 branch/worktree 和 commit 属于当前 item；
- 未跟踪和未提交文件清单；
- 有效修改存在，或任务显式允许 no-op；
- 推送后远端 ref 指向 `head_sha`；
- Gate 验证的 SHA 与待合并 SHA 一致。

不要用模型的“我已经完成”“tests passed”代替机器观测。

### P0-5. 远端 Git 提交与 Mac 上的 push 不是同一步

**原稿现状**：worker（例如 LXC）本地 commit，Orca 在 Mac 收到 success 后执行 `git push origin work/<id>`。Mac 本地不一定有那个对象或 ref，因此命令不一定成立。

**建议**：

1. Worker 在受限 worktree 内修改并 commit，不直接执行发布动作。
2. Host-side trusted wrapper 校验 ref、SHA 和 dirty 状态。
3. Orca 授权对应 host 执行严格限定的 `git push`，或让 Mac 显式 fetch 远端 commit 后再 push（需要可信传输和命名规则）。
4. Orca `git ls-remote` 验证远端 branch SHA 后启动远程 Gate。
5. 合并前再次比较目标分支 HEAD 与该 verdict 的 base；必要时重新执行 integration verification。

Git 发布凭据不应暴露给普通 worker 的 Agent shell；需要验证受限 workspace sandbox 是否允许必要的 Git metadata 写入（worktree 的 `.git` 文件可能指向主仓库 common dir）。

### P0-6. 无人值守执行必须显式启用沙箱

ADR 0043：Radian 不带 `--profile` 时缺省 `full`，没有沙箱，不提示。原 Orca 示例只有 `--yes`，因此不构成限制。

应规定：

```bash
radian exec -p - \
  --session-id "$session_id" \
  --workspace "$worktree" \
  --profile workspace \
  --yes \
  --output jsonl
```

这是示例契约，实际落地前仍应验证 sandbox + worktree Git commit、构建缓存路径、网络许可、`.git` 写入边界。超权限需要显式批准，不应自动退到 `full`。同时区分“Agent 可修改自己 worktree”和“人类 attach 只读观察”。

---

## 4. P1：应在 v0 文档钉住的边界

| 编号 | 问题 | 建议 |
|---|---|---|
| P1-1 | “一个 item 一个 session”过于刚性 | 保留一个活跃 session 的不变量；分开 item、session、attempt ID。可恢复错误优先 resume，poisoned/不可恢复时暂停或显式新 session，记录 lineage。 |
| P1-2 | 任意失败直接 `--resume` | 分类为基础设施失败、模型失败、验证失败、取消；不同策略；不把人为 cancel 当可自动重试错误。 |
| P1-3 | `stall_timeout` 仅按 JSONL 无新行 | 这是可操作信号，但静默模型推理也可能没有输出；结合绝对时间/成本上限和分阶段 timeout。 |
| P1-4 | Gate 允许 brief 自带任意 shell 命令 | 可信 gate profile 和明确审核；brief 只能引用允许的命令或申请例外；Gate 在隔离环境执行。 |
| P1-5 | Gate 号称“只读”但需运行构建 | 源码快照只读，不代表 filesystem 全只读；构建产物、缓存、temp 要有单独可写位置与资源界限。 |
| P1-6 | 不同主机上同时跑 Verify 和 mutation | 仍会有 shared cache/CPU/内存竞争；关键任务设 exclusive；mutation 和 commands 不共享可变构建状态。 |
| P1-7 | CLI 有 `cancel`，未定义远端取消 | 限定 attempt；SIGTERM 后宽限期升级；等待远端确认；仍可能 orphaned；取消后的旧结果不可覆盖状态。 |
| P1-8 | 任务级预算缺失 | 最多 attempts、run time、累计实际成本/token、总 Gate 资源；达到上限转 hold 而不是继续自动循环。 |
| P1-9 | `run` 排空即退与等待 human/CI/退避冲突 | 定义为“推进到没有立即可执行工作、且本次主动运行的阶段已收束”，不为 human approval、CI 或 retry_until 持续阻塞。 |
| P1-10 | `retry_after` 纯内存定时器 | 持久化绝对 UTC 到期时间；重启后重新判定；不能保存不可跨重启解释的 monotonic instant。 |
| P1-11 | Host 配置与 SSH 参数结构不清 | `ssh` endpoint、port、用户名、仓库路径应独立字段并安全构造 argv，不能把用户可控命令拼入单段 shell 字符串。 |
| P1-12 | 结果只记最后 `Result` 行 | 被 SIGKILL、SSH 断开、stdout 断流时可能没有最后一行；定义无 Result 的 attempt 为 unknown，需要对账，不接受部分 stdout 作为 success。 |

### 4.1 Session / Attempt 语义建议

原始不变量是 `session_id = item_id`，每次返工在原 host `--resume`。这对正常返工很简洁，但 ADR 0044 存在 `poisoned` 等不能无条件恢复的结局。

建议改写为：

> 一个 item 在任一时刻最多有一个有效的活跃 worker attempt；attempt 绑定真实 session 和 host。正常返工尽可能复用 session，不能安全恢复时进入 hold，并由人工授权创建 replacement session。

示例：

```text
item_id     = orca-rad-104
attempt_id  = orca-rad-104-02
session_id  = rad-104-a
host        = lxc
```

这只是身份分离建议；如果更希望贯彻绝对“一 item 一 session”，也可以将不可恢复的 session 转为 `failed`，但应接受需要人工重新建 item 的运维成本。

### 4.2 `radian attach` 不是 JSONL 远程观察 API

ADR 0043 的 `attach` 复用 TUI transcript 渲染，读取 journal 的已提交消息；不显示 streaming delta，也没有完整的 durable permission-wait 事件。当前控制口 `subscribe` 的事件也不包含模型 live delta。

建议 Orca 自己存储 `radian exec --output jsonl` 的执行流，保留 `live` 与 durable 事件区别；v1 UI 对 Orca 的事件订阅 API 读数据，不对 TUI 文本做解析。**无需为 v0 强制修改 `radian attach`。**

### 4.3 Result 用量可能被重复计算

ADR 0044 当前 `turns/usage/cost` 是 session 累计（包括前一次 resume）。`docs/backlog/exec-result-run-usage.md` 已决定新增 `run_turns/run_usage/run_cost`，但仍是 backlog 项；不能当作已经提供的字段。

在字段落地前，每个 attempt 的消费量应按新增 journal 记录独立计算；不能简单地把连续 `Result.cost` 相加。

---

## 5. 三份文档直接冲突与质量问题

| 来源 | 问题 | 推荐处理 |
|---|---|---|
| `orca.md` 验收 vs v0 设计 | Backlog 写 commands/mutation 不同树、**同机同时**；设计稿要求不同树、**不同时** | 以隔离、避免资源争用为原则，修正 backlog 验收。 |
| Gate 章节 vs 用户使用流程 | Gate 全绿直接 done；用户流程等 merge 后 done | Gate 全绿进入 review.ready_to_merge；merge 后 done。 |
| MCP 入口 vs `orca run` | 两个入口都能写 backlog，但只有一个长时间排他锁 | 明确短事务锁和状态版本。 |
| `doing/` 重启 | 检查 PID 就能接管远程会话 | 改为 attempt 远程对账；未知状态不得重派。 |
| SSH worker vs push | Worker commit 仅在远端，Orca 本地没有 ref | host-side trusted push / 明确 fetch。 |
| `human` vs non-daemon | 等人答复不符合“排空即退” | Human wait durable，run 退出，后续唤醒继续。 |
| `pass/fail` vs MCP | CLI 使用 pass/fail，但 MCP 只列 add/list/show/rework/cancel | 单独定义权限来源；lead 不能冒充 owner 批准。 |
| `Result` 与完成 | Radian completed 就移到 review | 额外独立核验 commit/dirty/ref/SHA。 |
| `meta.toml` 与 `runs/<n>` | 一个 `meta.toml` 容纳多次 attempt，缺少按轮身份 | `attempts/<id>.json` 或记录数组，不能覆盖历史。 |
| v1 “数据格式就是 API” | 内部 frontmatter 尚未定义 schema | v0 定最小稳定语义；schema_version 演进，不冻结整个内部布局。 |

**额外配置问题**：设计稿 `orca.toml` 示例将 `[hosts.mac] ssh=... path=... max=1` 写在同一行，并在 `[hosts.pc]` 后直接写 `stall_timeout`、`gates`。这不是可直接复制的合法/预期 TOML 顶层配置：每个赋值应独占行，跨表要明确放置，避免全局配置意外落入 `hosts.pc` 表。SSH endpoint/port 也应分别配置，而非将 `-p 2222` 拼进一个含空格的目标字符串。

---

## 6. 竞品格局与逐项核查

### 6.1 分类

| 类型 | 典型项目 | 主要价值 | 对 Orca 的启发 |
|---|---|---|---|
| 终端管理/多路复用 | herdr、cmux、claude-squad | pane、生存期、观察、交互 | Orca 不应回去做终端复用器。 |
| Agent 开发环境 | [onorca.dev Orca](https://www.onorca.dev/)、Conductor、Emdash | worktree、终端、diff、远端执行、App | Web/iOS 体验和工作区视图已有成熟参考。 |
| 任务编排与交付 | [Symphony](https://github.com/openai/symphony)、[Sortie](https://github.com/sortie-ai/sortie)、[OrchestratorInc](https://github.com/OrchestratorInc/agent-orchestrator) | tracker、调度、隔离、状态恢复、CI/review 循环 | **Orca 的直接竞争领域**。 |
| 多 Agent 组织/持久化 | Gas Town + [Beads](https://github.com/gastownhall/beads)、[Paperclip](https://github.com/paperclipai/paperclip) | 原子领取、合并队列、预算、审批、角色治理 | 借用局部机制，不复制复杂组织层。 |

### 6.2 Symphony：最值得学习状态机

官方 [SPEC.md](https://github.com/openai/symphony/blob/main/SPEC.md) 明确将 tracker state 与 orchestrator claim state 区分，包含 `Unclaimed/Claimed/Running/RetryQueued/Released`、并发限制、workspace 生命周期、指数退避、重启后 reconcile 和结构化可观察性。文档还有可选 SSH remote execution 附录。

**应学习**：只有编排器改调度状态；workspace 身份确定；worker 不直接改 task claim；失败回到明确的队列与重试时钟；重启后从权威状态重新对账。

**不要照搬**：强制长期运行服务、持续 tracker polling、Codex app-server 细节。Orca 的本地目录 backlog 是合理的另一个控制面选择。

### 6.3 Sortie：对“是否自己造轮子”的最直接挑战

[官方仓库](https://github.com/sortie-ai/sortie)、[Go Packages](https://pkg.go.dev/github.com/sortie-ai/sortie) 和 [官方文档](https://docs.sortie-ai.com/) 显示，Sortie 不只是早期单 Go 二进制脚本。2026-10-18 有 v1.28.1 的模块记录（**这是确认存在的版本，不在此声称它必然是今天最新版本**）。公开文档列有多 issue tracker 和 agent adapter、持久运行历史、成本、独立工作区、CI/review feedback。

重要的新核查：

- [Stage Chains](https://docs.sortie-ai.com/concepts/stage-chains/)：可以在固定阶段之间切换 prompt、agent 和模型；每个阶段启新 session，工作区延续。它是**线性**阶段链，并非任意 DAG 或 review 自动回跳。
- [Self-Review](https://docs.sortie-ai.com/guides/configure-self-review/)：在 worker 退出之前，运行 verification commands、生成 diff，再由 agent 给 verdict/继续修复；官方 workflow specification 当前说明默认 reviewer 是 `same` session。
- [CI Feedback / Review Comments](https://github.com/sortie-ai/sortie/blob/main/docs/architecture/05-workflow-specification.md)：支持有限次数的 CI 修复及审查意见回送，并设置 escalation。

**结论**：原竞品报告把 Sortie 说成“仓库未开”、且将独立检查笼统表述为“没人做”，需要改正。Orca 不能靠“有 review/返工”作为差异点。

### 6.4 onorca.dev Orca：最接近未来客户端体验

[产品官网](https://www.onorca.dev/) 明确介绍了 Git worktree、远端 SSH worktrees、终端、diff 批注、GitHub/Linear 集成、多种 CLI agent 和 iOS/Android 伴侣 App。

产品模式仍然偏**交互式 Agent Development Environment**：人或 Agent 管理工作区，直接看 diff 和终端。你的设计则偏**无人值守任务执行和交付约束**。二者有明显功能交叉，尤其在远程观察、Git 和跨端操作上。

**对 v1 UI 的建议**：展示 item 状态、所在 host、attempt、session 观察流、固定 SHA diff、验证证据、待审批动作；不宜仅把 UI 定义为 Markdown 目录查看器。

### 6.5 OrchestratorInc：必须承认已有独立 reviewer

[Review Loop 官方指南](https://docs.orchestrator.inc/guides/review-loop/) 写明可对 worker PR 启动专用 reviewer agent；reviewer 不拥有 worker 分支，可将评论回送给 worker。

因此“市场上没人做独立 Gate agent”不成立。Orca 的差别只能是独立验证执行的**强约束契约**：具体 SHA、源代码隔离、固定命令、原始输出、阶段 verdict 审计和结果失效规则，而不是“使用另一个 Agent 做 review”。

### 6.6 Gas Town / Beads：学习合并和任务一致性

[Beads 官方仓库](https://github.com/gastownhall/beads) 目前以 Dolt 为源数据，默认 embedded 单写者模式，server 模式支持多个写者；JSONL 作为 export/interchange，而不是权威源。它有原子 `bd update --claim`、依赖图和远端 sync。

从它的演化**不能**推导出“文件 + Git 绝对不可行”。准确结论是：**多个 agent 在各自分支同时修改同一结构化任务状态，并通过 Git 合并解决并发，是高复杂度设计**。Orca 的单写者目录可以避免这个问题，无需为了模仿 Beads 引入 Dolt。

Gas Town 的 Refinery/merge queue 方向更有启发：并行验证通过的两个分支不等于都能对不断更新的 main 安全合并。v0 可用简化的 merge 前对账和关键测试复验，不必照搬复杂的多 agent 组织架构。

### 6.7 Paperclip：治理与成本

[Paperclip 预算说明](https://github.com/paperclipai/paperclip/blob/master/docs/guides/board-operator/costs-and-budgets.md) 和 [Agent 管理文档](https://github.com/paperclipai/paperclip/blob/master/docs/guides/board-operator/managing-agents.md) 公开描述了预算警告、超额暂停、Agent heartbeat 和审批。

Orca 不需要公司组织结构，但应有任务级 `max_attempts`、`max_cost`、`max_wall_time`、必要时暂停等待 human。无人值守 agent 的开销、资源竞争和重复返工本身就是可靠性问题。

### 6.8 能力对照（2026-10-20，公开文档层面）

> `是` 表示有明确文档；`计划` 表示仅 Orca 设计；`未确认` 表示本报告未取得充分材料，不能推定为不支持。

| 能力 | Orca 设计 | Symphony | Sortie | onorca.dev Orca | OrchestratorInc | Paperclip |
|---|---|---|---|---|---|---|
| 确定性任务调度 | 计划 | 是 | 是 | 部分重叠 | 是 | 是 |
| 工作区隔离 | 计划 | 是 | 是 | 是 | 是 | 依适配器 |
| SSH/远程执行 | 计划 | 可选规范 | 有公开相关设计 | 是 | 有远端能力 | 依部署 |
| 自定义阶段链 | 计划 | 外部 workflow | 是，线性链 | 未确认 | 有工作流 | 有审核阶段 |
| CI/review 返工 | 计划 | 交 workflow | 是 | 可人工操作 | 是 | 有审查治理 |
| 独立 reviewer agent | 计划 | 非规范重点 | v1 self-review 为同 session | 未确认 | **是** | 未确认 |
| 固定 SHA 的独立 commands gate | 计划 | 非规范核心 | 有验证命令，等价性待核 | 未确认 | 细节待核 | 未确认 |
| Mutation gate | 计划 | 未见规范内置 | 未确认 | 未确认 | 未确认 | 未确认 |
| 一次性排空即退 | **计划** | 否，长期服务 | 长期调度倾向 | 非核心 | 非核心 | heartbeat/服务 |
| 移动端 App | v1 计划 | 非规范重点 | 未确认 | **是** | 有多端产品 | Dashboard |
| 任务/Agent 预算 | 待设计 | 不作为主规范重点 | 成本追踪 | 使用量展示 | 未核全 | 是 |

---

## 7. 真正可以验证的 Orca 差异化

不要把“调多个 Agent”作为产品的核心卖点。更具体的定位应是：

> **Agent Delivery Controller**：对一个 work item 的运行、代码产物、验证证据、重试和人工交付维护可恢复的一致性状态，确保对某个指定 commit 的验证结果不会被错误复用到另一个 commit。

衡量差异时应该问：

1. 执行结果是否可关联到确切的 `item_id/attempt_id/session_id`？
2. Gate 判定是否指向**不可变的 commit SHA**？
3. worker 失败、SSH 中断、Orca 被杀后，是否会重复派发？
4. 人工批准后，合并之前代码变了，旧批准是否失效？
5. 两个原本独立通过测试的分支依次合入 main，是否复验新集成结果？
6. 每项任务的模型和验证成本是否能够界定？
7. 启动和维护是否足够简单，值得取代已有 CLI + CI 流程？

**重要的保留意见**：固定 SHA 的验证本身可通过 CI、合并队列或现有编排器实现。它是 Orca 的**设计关注点**，不是自动成立的市场独占能力。

---

## 8. 建议的最小 Orca v0.1 架构

```text
                 Claude / Lead
             plan · add · rework
                      |
                 MCP / CLI
                      |
              +----------------+
              | Orca Control   |
              | - transactions |
              | - scheduler    |
              | - reconcile    |
              | - gate state   |
              +--------+-------+
                       |
                   SSH/stdio
             +---------+---------+
             |         |         |
          macOS       LXC        PC
          Radian     Radian     Radian
          worktree   worktree   worktree
             |         |         |
             +---------+---------+
                       |
           commit SHA / Result JSONL
                       |
           isolated commands gate
                       |
            optional review gate
                       |
                human approval
                       |
          integrate & verify merge
```

### 8.1 五个目录不变，补子状态

```text
<backlog>/
  lock
  todo/<item>.md
  doing/<item>.md
  review/<item>.md
  done/<item>.md
  failed/<item>.md
  runs/<item>/attempts/<attempt-id>/
      result.jsonl
      stderr.log
      execution.json
      commands.verdict.json
      review.verdict.json
```

目录决定**粗状态**，frontmatter/metadata 决定细阶段：

| 目录 | phase 示例 | 是否占 worker slot |
|---|---|---|
| todo | `ready`, `blocked`, `retry_wait` | 否 |
| doing | `starting`, `running`, `reconciling` | 视真实远端运行状态 |
| review | `verifying`, `awaiting_human`, `rework_pending`, `ready_to_merge` | 只有真正运行 Gate 时占相应资源 |
| done | `merged` | 否 |
| failed | `exhausted`, `cancelled`, `irrecoverable` | 否 |

**状态转换原则**：

- `todo.ready -> doing.running`：原子 claim，绑定 attempt token。
- `doing.running -> review.verifying`：收到**合法** Result，检查当前 attempt 与 commit。
- `doing.running -> todo.retry_wait`：仅在确认执行已终止且失败可重试时；记录下次触发时刻。
- `doing.running -> doing.reconciling`：执行状态未知；不自动重派。
- `review.verifying -> review.awaiting_human`：所有 required Gate 通过；人工决定前不可算 done。
- `review.awaiting_human -> review.ready_to_merge`：owner 对绑定 SHA 批准。
- `review.ready_to_merge -> done.merged`：集成成功，且符合目标 HEAD/SHA 验证契约。
- `review.* -> todo.ready`：人工/lead 提交明确的 rework brief，并增加 attempt 编号。
- 重试耗尽、无法恢复或明确 cancel → `failed`（或需要调查时 hold，不能冒称正常失败）。

### 8.2 示例任务数据

```yaml
schema_version: 1
id: orca-rad-104
title: Fix session recovery
state: review
phase: awaiting_human
after: []
execution:
  host: lxc
  session_id: rad-104-a
  attempt_id: orca-rad-104-02
  base_sha: abc123
  head_sha: def456
verification:
  verified_sha: def456
  commands: passed
  review: passed
  mutation: skipped
limits:
  max_attempts: 3
  max_cost_usd: 5.00
```

该 YAML 为**推荐语义示例**，不要求一定使用 YAML 存储。真正的 v0 可以继续 Markdown + TOML，但必须定义 schema version、缺省值、转移规则和损坏/旧版本处理。

### 8.3 Gate 分成三层

**第一层：Deterministic Verification（v0 required）**  
在固定 `head_sha` 的隔离 checkout/worktree 上执行来自可信配置的 commands；保存命令、退出码、环境标识和必要输出。对变异测试，只作为可选实验阶段，独立源码树、独立构建状态、严格回滚与超时。

**第二层：Independent Review（v0 可选）**  
只读 reviewer 基于 brief、diff、Result 与测试结果给出结构化 blocking/suggestion 信息；review 是意见，不替代 commands 真值。与 worker 同 session 的 self-review 不应冒充独立 review。

**第三层：Human Handoff（v0 required）**  
人类对**指定 SHA + 指定 Gate verdict 版本**审批。未经授权不能由 Claude 通过 MCP 伪造 owner pass；合并前 SHA 或 base 改变，进入复验/重新批准逻辑。

CI 可看作第一层的远端执行后端；不必在 v0 中为其再设计一整套独立 DSL。

### 8.4 `orca run` 的严格语义

`orca run`：读取状态、执行所有当前可推进且符合容量限制的任务、收束本次发起的工作、持久化结果，然后在**没有立即可执行步骤**时退出。

- `retry_wait` 任务等待下一次定时唤醒。
- `awaiting_human` 等人操作，不堵住该进程。
- CI 长时间等待应持久化 pending 状态，由后续 `run`/webhook/serve 重新对账。
- 无法确认远端执行状态时保持 `reconciling`，不强行视为失败。

这样才能保留“非 daemon”的真实产品意义，同时不假装定时器与长时服务不存在。

---

## 9. 实施范围：建议先删掉的东西

1. **Mutation 不是默认 required**：先证明执行、验证、返工与合并闭环正确，再通过实验评价 mutation 是否实质提高缺陷发现率。
2. **不做通用 Gate DSL**：v0 固定 `commands -> [review] -> human` profiles；可选阶段通过可信配置开关。
3. **不做跨机器 session resume**：正常返工固定原 host；失联后保守等待人工对账。
4. **不做模型自主生成且直接执行下一轮 rework**：失败报告给 lead，让它提交新 brief，再由 Orca 有界执行。
5. **不做负载预测调度**：全局/host 静态 slots；有必要的 Gate 设 exclusive。
6. **不做 UI 之前过度冻结所有文件字段**：先冻结 identity、state、attempt、verification SHA，其他使用 schema version 演进。
7. **不并入 `radian` 二进制**：先单独目录/仓库进行 adapter 验证；确定值得维护后再讨论 CLI、ADR 0032/0043 的更改。
8. **不复制 Gas Town 的组织角色、数据库和合并代理队列**：按实际并发冲突增长再引入必要机制。

---

## 10. 建议的实施/实验顺序

| 阶段 | 范围 | 必过验收 |
|---|---|---|
| Spike 0 | 本地 Radian exec JSONL adapter | 识别完整 Result；无 Result 不算成功；记录 commit/dirty 状态。 |
| Spike 1 | backlog + 原子状态事务 | 并发 `add/run/cancel` 不丢状态、不重复 claim；支持重启。 |
| Spike 2 | SSH + attempt + reconcile | SSH 断线但远端仍跑时不重复派发；旧 attempt 结果无法污染新状态。 |
| Spike 3 | trusted Git push + 固定 SHA commands gate | 真正确认远端 branch SHA；Gate 只认可当前 SHA；能检出验证结果。 |
| Spike 4 | bounded retry + human gate + merge safety | 人工审批等待期间 `run` 能退出；main 更新后旧 Gate 不被错误复用。 |
| v0 试验 | 一个真实多 item、跨两 Host 需求 | 全程不用 herdr；失败、返工、预算、合并记录可追溯。 |

### 必须执行的故障注入测试

- 两个 `orca run` 并发，只有一个能领取同一 item。
- 运行中 MCP 添加任务，不破坏正在持久化的结果。
- Worker 已产生 commit、Orca 未收到 Result 就被杀；恢复不得覆盖或丢弃 commit。
- SSH 被断开，远端 worker 继续执行；不得重开第二个 worker。
- 远端同 PID 被复用，Orca 不能把它认成旧 attempt。
- Worker exit 0 但工作区 dirty 或根本无产出；不能直接验证通过。
- Gate 已验证 SHA A，worker 把分支推到 SHA B；旧 verdict 作废。
- A、B 从相同旧 main 分叉；A 合入后对 B 进行集成检查或复验。
- Mutation 或 Verify 修改共享缓存时，不影响独立验证结论。
- Human pass 对象 SHA 改变时，不允许使用旧审批合并。
- Budget/attempt 耗尽必须停止，不出现无限返工。
- `run` 在 human/CI/retry 等待时可以正常退出，下一次能恢复。

### 建议用于比较“Sortie 适配”与“自建 Orca”的指标

- 部署步骤/依赖数量、常驻服务复杂度。
- 相同任务从 brief 到 PR/merge 的人工操作次数。
- 远端断连、kill -9、Git 冲突三类故障的恢复正确率。
- 单个 item 的总 token/cost 与 Gate 额外开销。
- 验证覆盖：有多少问题由 CI/commands/review/mutation 分别发现。
- 代码量、维护面和适配未来 Radian CLI 契约的变更成本。

---

## 11. “自建 Orca”还是“Sortie + Radian adapter”？

| 你的首要目标 | 倾向 |
|---|---|
| 尽快把 GitHub Issues 派给多个 Agent，自动产 PR、收 CI/Review 反馈 | **先试 Sortie**，确认 adapter 兼容性。 |
| 完全本地文件 backlog、Claude MCP 操作、不依赖 issue tracker | **自建极简 Orca Runner** 更贴合。 |
| 命令用完即退出，不管理全天候服务 | **Orca** 的简化执行模式有价值。 |
| 最关注指定 SHA 的隔离验证、审批与可重复交付证据 | 先实现**独立 Orca Gate 原型**，可以为不同 Runner 服务。 |
| 立即拥有成熟远程 Dashboard 和跨端操作 | 复用已有管理器更快，不建议 v0 同时自研 App。 |

**推荐一次双路径短实验，而不是立即完整自建：**

- **路径 A**：Sortie + 最薄 Radian adapter，在真实 issue 上测试并发、重试、CI/review feedback。
- **路径 B**：极简 Orca Runner + 独立 Gate，在本地 Markdown backlog 上测试相同需求，尤其 SSH 故障恢复和指定 SHA 的 Gate 语义。

比较结果再决定是否长期维护完整 Orca。这不是要放弃 Orca，而是避免为了 Gate 的差异化目标，无意中再造一整套已有成熟产品的调度和 Dashboard。

### 可以长期保持独立的两层

- **Orca Runner**：工作项、调度、SSH/stdio、attempt、生命周期及对账。
- **Orca Gate**：固定 SHA、受信任验收命令、隔离执行、verdict、人工批准及交付前失效规则。

Gate 尽量不依赖某个唯一调度器；将来由 Orca Runner、Sortie 或 CI 调用，仍应对同一个 commit 得到可比的证据链。

---

## 12. 对三份文档的具体修改清单

### `docs/backlog/orca.md`

- [ ] 保持 `idea / 不开工` 状态，不把审核建议当成 owner 已拍板。
- [ ] 修正 commands/mutation “同机同时”与设计稿“不同时”的矛盾。
- [ ] 加 `item_id/session_id/attempt_id`、合并完成定义和 bounded retries。
- [ ] 明确无人值守 worker 必须显式 `--profile workspace`。
- [ ] 新增 SSH 中断、远端结果未知、固定 SHA 失效与 merge 前检查的验收条款。

### `docs/research/2026-10-20-orca-v0-design.md`

- [ ] 将“进程单写者”改为“事务串行化 + 状态版本”，明确 lock scope/durability。
- [ ] 定义 `reconciling`，删除“按 PID 可直接接管”承诺。
- [ ] 定义 verified/approved/merged 及 `done` 的唯一语义。
- [ ] 分离运行成功、代码产物成功与交付成功。
- [ ] 补 host-side trusted push 和 push SHA 校验。
- [ ] Gate 默认只 required commands + human，mutation/review 可配置。
- [ ] 明确 Gate command 的可信来源、沙箱、可写缓存和独立测试目录。
- [ ] 修正 `orca.toml` 的 TOML 语法、配置作用域和 SSH 参数结构。
- [ ] 把 `radian attach` 与 `radian exec --output jsonl` 的职责分开。
- [ ] 用 per-attempt 增量计费，不重复累计 `Result.cost`。
- [ ] 定义 `run` 排空与 pending human/CI/retry 的关系。

### `docs/research/2026-10-20-orca-competitors.md`

- [ ] 更新 Sortie 源码仓库状态、至少 v1.28.1 的版本事实与 Apache-2.0 许可证。
- [ ] 增补 Stage Chains、Self-Review、CI/Review Feedback；指出 v1 reviewer 默认同 session。
- [ ] 将 OrchestratorInc 的独立 reviewer 纳入最近竞品列表。
- [ ] 将 onorca.dev Orca 的 SSH worktree、移动伴侣和 diff/CI 交互更新到能力表。
- [ ] 删除“没有竞品独立 Gate Agent”的绝对判断。
- [ ] 将 Beads 案例收敛为“多写者 + Git 合并任务状态的风险”，不下“文件进 Git 是死路”的绝对结论。
- [ ] 重写差异化：固定 commit 验证、恢复安全、审计、非 daemon 的成本/收益。
- [ ] 补一次性原型 vs Sortie adapter 的选择试验。

---

## 13. 结论与建议拍板

**结论 A：架构保留。** Radian 不应重新内置多 Agent 工作流；Orca 独立控制多个一次性 Agent 的交付状态，是合理的分层。

**结论 B：v0 方案先修再做。** 优先修补锁事务、远端 attempt 恢复、Git 发布、`done` 定义、sandbox 和固定 SHA Gate。这些是正确性问题，不属于可留到 v1 的体验优化。

**结论 C：降低 v0 复杂度。** v0 应证明可恢复地完成一次实际 coding delivery，不需要一开始实现 mutation 平台、复杂 Gate DSL、跨机器 resume、完整 Web/iOS 客户端。

**结论 D：竞品研究的焦点改为实现差异而不是功能名。** Symphony 已经提供成熟的调度规范；Sortie 已经提供丰富的 adapter、阶段链、验证和反馈；OrchestratorInc 有专用 reviewer；onorca.dev Orca 已经覆盖 SSH 和移动观察。Orca 的意义要通过更清楚的交付契约、更低的本地使用成本和实际可靠性来证明。

**我建议的下一步**：先修三篇文档并保持 `idea` 状态；随后做一个可抛弃的端到端原型，同时对照 Sortie adapter。只有当独立的 Orca Runner / Gate 在真实需求中明显降低人工介入并提高恢复正确性，再考虑进入 Radian 主产品或规划桌面与 iOS 客户端。

---

## 参考资料（原始文档与官方资料优先）

### Radian

1. [`docs/backlog/orca.md`](https://github.com/hisea/radian/blob/main/docs/backlog/orca.md)
2. [`docs/research/2026-10-20-orca-v0-design.md`](https://github.com/hisea/radian/blob/main/docs/research/2026-10-20-orca-v0-design.md)
3. [`docs/research/2026-10-20-orca-competitors.md`](https://github.com/hisea/radian/blob/main/docs/research/2026-10-20-orca-competitors.md)
4. [ADR 0043](https://github.com/hisea/radian/blob/main/docs/adr/0043-single-session-radian.md) · [ADR 0044](https://github.com/hisea/radian/blob/main/docs/adr/0044-exec-jsonl-output.md) · [ADR 0032](https://github.com/hisea/radian/blob/main/docs/adr/0032-one-binary.md) · [ADR 0022](https://github.com/hisea/radian/blob/main/docs/adr/0022-session-fork.md)
5. [Radian Host Control Protocol](https://github.com/hisea/radian/blob/main/crates/radian-host/CONTROL.md)
6. [Result 增量用量 backlog](https://github.com/hisea/radian/blob/main/docs/backlog/exec-result-run-usage.md)

### 竞品

7. [OpenAI Symphony SPEC](https://github.com/openai/symphony/blob/main/SPEC.md)
8. [Sortie GitHub](https://github.com/sortie-ai/sortie) · [Go Packages](https://pkg.go.dev/github.com/sortie-ai/sortie)
9. [Sortie Stage Chains](https://docs.sortie-ai.com/concepts/stage-chains/) · [Sortie Self-Review](https://docs.sortie-ai.com/guides/configure-self-review/)
10. [Sortie Workflow Specification](https://github.com/sortie-ai/sortie/blob/main/docs/architecture/05-workflow-specification.md)
11. [Orca ADE (onorca.dev)](https://www.onorca.dev/)
12. [OrchestratorInc Review Loop](https://docs.orchestrator.inc/guides/review-loop/)
13. [Beads](https://github.com/gastownhall/beads)
14. [Paperclip](https://github.com/paperclipai/paperclip) · [Costs and Budgets](https://github.com/paperclipai/paperclip/blob/master/docs/guides/board-operator/costs-and-budgets.md)

---

*本报告是对 2026-10-20 所读文档和公开资料的设计审查，不是代码测试报告；建议项均待项目 owner 最终确认。*

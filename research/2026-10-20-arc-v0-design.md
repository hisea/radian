# arc v0 设计稿（修订版，2026-10-20）

状态：**定为 v0，作为 idea 记录，不开工。** 本版按同日两轮外部审核修订（第一轮
`2026-10-20-arc-v0-review-and-competitor-analysis.md`，第二轮 owner 转达的复审）：两轮 P0 全部采纳，
P1 与文档矛盾按本文"修订记录"一节处理。替代 herdr；不是 daemon。将来要 desktop / iOS / web app（见末节）。

## 角色

- **Claude（lead）**：分析需求、制定计划、拆成 item，经 arc 的 MCP 工具写入 backlog；看 gate 判定和
  diff，决定返工（提交新的 rework brief）；**不能**替 owner 批准。
- **arc（控制面）**：backlog 目录状态变更的**唯一写者**。接收 item、按依赖和容量派发、对账
  attempt、核验交付产物、起 gate、记录预算。确定性程序，不是模型。
- **worker**：一次性 `radian exec -p`，一个 item 一个 session、一个 worktree（单机 v0 共享一个 clone，owner
  决定，见派发）；跑完退出；工作区可写、
  **无交互入口**（headless 无 prompter，`ask_user` 不挂载，ADR 0043 §4）；显式 `--profile workspace`。
- **gate**：可配置的阶段序列（commands / mutation / review / ci / human），每个阶段一次性运行、绑定
  一个 `verified_sha`，写判定。机械部分归它，判断归 Claude，批准与 merge 归 owner。
- **人类**：`radian attach <session-id> [--follow]` 只读观察已提交消息（ADR 0043 §7；无流式
  delta）；`arc show` 读 backlog；`arc pass/fail` 是 owner 权限。**v0 这是流程保证，不是安全边界**：lead 与 arc 同一 OS 用户，
  shell 里能跑 `arc pass`，也能改 backlog 文件；v0 只靠不给 lead 暴露审批工具。真正的授权边界留到
  `arc serve` 有独立身份（独立用户或受保护的控制进程）时再做，在那之前不把 "owner-only" 写成不变量。

## 三个身份

| 身份 | 规则 |
|---|---|
| `item_id` | backlog 文件名，永不变 |
| `session_id` | **= `item_id`**（一个 item 一个 session）。返工 `--resume` 回同一 host。`Result.outcome = poisoned` 等不可恢复时 item 进 `failed/irrecoverable`，人重新 `add`，新 item 的 frontmatter 写 `supersedes: <旧 item>`；原 session 不可变、不 fork |
| `attempt_id` | `<item_id>-<n>`，每次执行一个。记 host、随机 attempt token、远端进程身份（pid + 启动时间）、worktree 目录、branch、`base_sha`、`head_sha`、起止时间、session_id。旧 attempt 回来的结果因 token 不匹配被拒 |

## backlog：目录加 phase，单写者事务

```
<backlog>/
  state.lock                            # 短事务锁：每次状态变更
  runner.lock                           # 一次 `arc run` 的生命周期：同一时刻只有一个调度循环；MCP 不拿它
  todo/<item>.md                        # phase: ready | blocked | retry_wait(retry_at: <UTC>) | paused_budget
  doing/<item>.md                       # phase: starting | running | reconciling
  review/<item>.md                      # phase: verifying | rework_pending | awaiting_human | integrating | ready_to_merge
  done/<item>.md                        # phase: merged（done 的唯一定义）
  failed/<item>.md                      # phase: exhausted | cancelled | irrecoverable
  runs/<item>/attempts/<attempt-id>/
    execution.json                      # 上表 attempt 字段
    result.jsonl                        # worker stdout 全流（ADR 0044 的 journal 行、delta 行、Result），原样保留
    attempt_finished.json               # 包装器自己的终止记录（见下），不往 Radian 的 Result 里塞字段
    stderr.log
    <stage>.<gate_run_id>.verdict.json  # 每个 gate 阶段每次运行一份，含 verified_sha
    integration.<n>.json                # Integration Gate：base_sha、candidate_sha、结果
```

- item 文件 = frontmatter（`schema_version`、`id`、`state`、`phase`、`state_version`、`after`、`host`、
  `execution{...}`、`verification{verified_sha, verdict_set_digest, <stage>: passed|failed|skipped}`、
  `approval{sha, verdict_set_digest, by, at}`、`supersedes`、
  `limits{max_attempts, max_cost, max_wall_time}`）+ brief 正文。v0 冻结的字段只有 identity、
  state/phase、attempt、verification sha；其余按 `schema_version` 演进。
- **两把锁。** `state.lock`：单写者 = 任一状态变更事务只有一个持锁写入者，不是"同一时刻只有一个 arc
  进程"。事务：取锁 → 核 `state_version` → 写临时文件 → fsync → 同文件系统 rename → 父目录 fsync → 释
  放。ssh、git、gate 全在锁外。`runner.lock`：一次 `arc run` 从头持到尾，保证只有一个调度循环在分配
  容量、起远端进程、跑 gate；第二个 `run` 拿不到就退出。MCP server 只用 `state.lock`，所以 Claude 随时
  能 `add`。v1 的 `arc serve` 接管 `runner.lock` 的角色。
- worker 和 gate 不写这个目录、不知道它的路径；brief 永远是 stdin 的一份拷贝。
- 不依赖 git 分支；要历史就让 arc 把目录当单分支仓库，只有它 commit。

## 状态转移

| 从 → 到 | 条件 |
|---|---|
| `todo.ready → doing.starting` | after 全部 `done.merged`，host 有空位；原子 claim，生成 attempt |
| `doing.running → review.verifying` | 收到**合法**的 attempt 结束记录（token 匹配、含 Result），且交付核验通过（见下） |
| `doing.running → todo.retry_wait` | 确认执行已终止且失败可重试；`retry_at` 持久化为绝对 UTC |
| `doing.running → doing.reconciling` | 远端状态未知（ssh 断、无结束记录）；**不自动重派**，等人 |
| `review.verifying → review.awaiting_human` | 所有 required 阶段对同一 `verified_sha` 通过；记 `verdict_set_digest`（各阶段 verdict 的 gate_run_id 集合的摘要） |
| `review.awaiting_human → review.integrating` | owner `pass <item> <sha>`：sha 必须等于 `verified_sha`，且当前 `verdict_set_digest` 等于 pass 时看到的那个；同一 sha 被重审（digest 变）后旧 pass 作废 |
| `review.integrating → review.ready_to_merge` | Integration Gate 通过（见 gate 一节）：记 `base_sha`（当时 main）与 `candidate_sha` |
| `review.ready_to_merge → done.merged` | 合并时核 main 仍是 `base_sha`，否则回 `integrating`；合并后核 `origin/main` 包含 `head_sha` |
| `doing.running → todo.paused_budget` | 预算（attempts / cost / wall time）耗尽。**不是** `awaiting_human`：此时没有可批准的产物。owner 的动作是 `reset-budget`，不是 `pass` |
| `review.* → todo.ready` | lead 或 owner 提交 rework brief；attempt 编号 +1 |
| 任一 → `failed` | 重试或预算耗尽（`exhausted`）、cancel、不可恢复 |

分支在 gate 之后变动（`head_sha` 变）→ 该 sha 的 verdict 与 approval 作废，回 `verifying`。`main` 在
Integration Gate 之后变动（不再是 `base_sha`）→ 回 `integrating` 重做；在旧 head 上复跑 `commands` 不算集成复验。

## 派发

v0 推（ssh + stdin），v1 拉（`radian work --pull`，需要 `arc serve`）；`radian work` 两种都接，它只收
一份 brief 加 item/attempt id。

### `radian work`（host 侧可信包装器，先是脚本）

1. 建或复用 linked worktree `.radian-worktrees/<item>` 与分支 `work/<item>`（首轮自 `origin/main`，记
   `base_sha`）。**决定：单机 v0 用 worktree 共享一个 clone**，不做每 item 独立 clone，不改
   Core 和沙箱规则；这是对第二轮审核 P0-1 的保留决定。已知风险（ADR 0038）：`workspace` 沙箱可写 refs、
   index、objects，linked worktree 共享 common dir，所以 worker A 在沙箱里能改 `refs/heads/work/item-B`。
   v0 的态度是**检测并安全失败，不预防**：
   - 远端 `origin/work/<item>` 是 push 之后的唯一真相；本地 ref 只是工作副本。
   - 包装器在 push 前核：`work/<item>` 存在、`head_sha` 是 `base_sha` 的后代、worktree 的 HEAD 指向这个
     branch、无 dirty。任一不成立 → attempt `failed`，不 push，保留现场；该 item 的返工从远端 ref 重建 worktree。
   - 包装器在 push 后核 `ls-remote` == `head_sha`；arc 再核一次。
   - 被别的 worker 改坏的 ref 在它自己的 attempt 结束时按上面被发现，损失是一次 attempt，不是静默交付错
     误；`git reflog` 可追溯（ADR 0038 §38 的同一前提）。
   - 多 host 或出现实际篡改案例时再评估独立 clone（objects 经 `--reference` 共享，refs 隔离），作为 Spike 0
     的对照项保留。
   worktree 目录上一把 flock 做同一 item 多个 attempt 的互斥（身份记录不是锁）。
   同时写 host-local attempt record `<host-run-dir>/<item>/attempts/<attempt-id>/{identity.json,status.json}`：
   随机 token、pid + 启动时间、session id、worktree 路径、预期 branch；结束时写终止结果。arc 重连后 ssh 读它
   并核进程身份。
2. 跑模型：
   ```
   radian exec -p - --session-id <item> --workspace <worktree> \
     --profile workspace --yes --output jsonl
   ```
   返工时加 `--resume <item>`。
3. 模型退出后，**包装器而不是模型**按顺序做：收 Result 与退出码 → 核 worktree 属于本 item、读 `head_sha`、
   列未提交和未跟踪文件、判断有无有效改动 → 分类：**只有 `outcome = completed` 且产物有效**才
   `git push origin work/<item>`，之后 `ls-remote` 核远端 ref；其余（`failed`、`incomplete`、
   `budget_exceeded`、`cancelled`、dirty、无产物）保留本地证据不 push。包装器的每条 git 命令带
   `-c core.hooksPath=` 置空、`GIT_CONFIG_NOSYSTEM`，只读自己的配置，不读模型可写的仓库内配置：
   ADR 0038 下 `.git/hooks` 模型改不了，但工作区内 `core.hooksPath` 目录（如 `.husky/`）可写且会被执行。
   push 凭据只在包装器的环境里；**两者同一 OS 用户时这不是隔离**，Spike 要核实沙箱是否真的遮住凭据路径
   （ADR 0020 禁读清单）。
4. stdout 最后一行是包装器自己的 `AttemptFinished`：`attempt_id, token, exit_code, outcome(来自 Result 或
   terminated), base_sha, head_sha, dirty[], pushed, remote_sha`。Radian 的 `Result` 原文另存，不改。
   **包装器确认子进程已退出但没有 Result**（崩溃、SIGKILL）→ `AttemptFinished.outcome = terminated`，
   arc 判 `failed`，不进 `reconciling`；只有连包装器都联系不上、远端状态无法证明时才 `reconciling`。
5. 退出码透传（ADR 0044 §3）。

### arc 侧

- 收到 `AttemptFinished` 后核 token 匹配当前 attempt，`git ls-remote origin work/<item>` 核远端 ref == `head_sha`，再进 `review`。
- **交付核验独立于 `Result.outcome`**：`completed` 只表示执行链结束（ADR 0044），不是任务完成。
  dirty 非空、无有效改动（且 brief 未允许 no-op）、sha 不符，都按失败处理。
- **并发**：全局与每 host 静态上限；gate 的 `commands`/`mutation` 标 exclusive。
- **stall**：jsonl 流 `stall_timeout` 内无新行 → SIGTERM（退出码 143，outcome `cancelled`），**并**叠
  加 `max_wall_time` 绝对上限；两者都记失败类别。
- **失败分类**：基础设施（ssh、host）、模型、验证、取消。只有前两类进指数退避重试（10s 起，上限
  5min，`max_attempts` 内）；取消不重试；验证失败回给 lead 写 rework，arc 不自动生成下一轮 brief。
- **预算**：`max_attempts`、`max_cost`、`max_wall_time`；到顶进 `todo.paused_budget`，不继续循环；owner
  `reset-budget` 才回 `ready`。批准继续花钱和批准交付是两种权限动作，不共用 phase。
  费用按 attempt 增量算：`Result.cost` 是会话累计（ADR 0044），`run_*` 字段落地前按本次新增的
  journal 行算。
- **取消**：限定 attempt；SIGTERM → 宽限 → SIGKILL；等远端确认；确认不了进 `reconciling`。
- **重启**：不留内存状态。先拿 `runner.lock`。扫 `doing/`：对每个 attempt ssh 读 host-local attempt record，
  核 token 与 pid + 启动时间：仍在跑 → 继续等；已写终止结果 → 按它处理；record 说在跑但进程不在 → 按
  `terminated` 失败；联系不上 host → `reconciling`。不按本地 ssh 子进程 pid 判断。
- **`arc run` 语义**：推进所有当前可做的转移，收束本次发起的工作，持久化，在没有立即可做的事时退出。
  `retry_wait`、`awaiting_human`、CI 等待都不堵它；下次由定时器或人再起。

### v1：拉

每 host 一个定时器跑 `radian work --pull <arc-url>`：向 arc 要一个 item；arc 作为唯一写者决定给不
给；给了就返回 brief 与 attempt id，脚本起新 session，跑完回传结束记录，退出。本机容量由脚本判断。

## gate：可配置的阶段序列

- 触发：item 进 `review.verifying`。每阶段一次性运行，绑定 `verified_sha`，产出自己的 verdict。
- 配置两级：arc 的缺省序列（profile）；brief 的 `gates:` 只能加严，不能删 `required`。
- **命令来源可信**：`commands` 阶段跑的是 profile 里的固定命令；brief 只能引用已允许的命令或申请例外，
  不能带任意 shell。

| 阶段 | 做什么 | 隔离 |
|---|---|---|
| `commands` | 对 `verified_sha` 的独立 checkout 跑验收命令（radian：Verify 七条，`--no-fail-fast`，易抖的重复 N 次），存命令、退出码、环境标识、尾部输出 | 独立 worktree；源码只读，构建产物与缓存有单独可写位置；exclusive |
| `mutation` | 按 brief 的 `mutations:`（行为 → 改坏方式 → 应红的测试二进制）逐个改坏、跑、恢复、比对 | 与 `commands` 不同树、不同时、不共享可变构建状态；exclusive |
| `review` | 只读 radian 会话读 diff、brief、Result、commands 结果；核对"保证"类注释；出 阻塞/建议/已核对 | 任意 host；不改代码；**不是** worker 同 session 的 self-review |
| `ci` | arc 推 PR，等 checks；视为 `commands` 的远端执行后端 | GitHub |
| `human` | owner 对 `verified_sha` + `verdict_set_digest` pass/fail | 无；durable，`run` 不等它 |
| `integration` | pass 之后、合并之前：读当前 main 为 `base_sha`，临时工作区 merge（或 rebase）`head_sha`，冲突即停并回给 lead 返工；对 `candidate_sha` 跑 profile 的关键命令；记 base/candidate/结果。合并时 main 必须仍是 `base_sha` | 独立工作区；exclusive。不是 merge queue，只是一次对预期合并结果的验证 |

- **缺省 profile**：通用 `commands(required) → human(required)`；**radian 仓库 profile**：
  `commands(required) → mutation(required) → review → human(required)`（AGENTS.md 今天就要求
  变异检查）。
- `on_fail`：`commands`/`mutation` 失败 → `rework_pending`（通知 lead，等它的 rework brief）；`review`
  阻塞 → `awaiting_human`。
- 判断归 Claude，批准归 owner：MCP 工具里没有 `pass/fail`（流程保证，见角色一节）。
- **只支持固定 profile**：没有条件表达式、没有动态 DAG、没有覆盖优先级；brief 的 `gates:` 只能在 profile 之上追加阶段。mutation 阶段复用仓库现有的验证脚本，arc 只负责隔离执行与收集结果，不做通用 mutation 框架。

## 配置（合法 TOML）

```toml
backlog = "~/.radian/arc/radian"
repo = "git@github.com:hisea/radian.git"
stall_timeout = "10m"
max_wall_time = "2h"
profile = "radian"

[hosts.mac]
ssh_host = "localhost"
path = "~/Code/radian"
max = 1

[hosts.lxc]
ssh_host = "<linux-host>"
ssh_user = "<user>"
path = "~/radian"
max = 2

[hosts.pc]
ssh_host = "<windows-host>"
ssh_user = "<user>"
ssh_port = 2222
path = "~/radian"
max = 1

[profiles.radian]
gates = ["commands:required", "mutation:required", "review", "human:required"]
commands = ["cargo fmt --all -- --check", "cargo clippy --workspace --all-targets -- -D warnings", "..."]
```

ssh 参数分字段，argv 安全构造，不拼单段 shell 字符串。

## 不做（v0）

- 常驻进程：`arc run` 排空即退；worker/gate 一次性。
- worker 中途插话：纠正走 rework。
- worker 写 backlog、读 backlog 路径、持有 push 凭据。
- 跨机器 resume：返工固定原 host；失联保守等人。
- 多个调度器同时运行：`runner.lock` 只放一个 `arc run`。
- 每 item 独立 clone：单机 v0 用 linked worktree 共享 clone；独立 clone 留作多 host 时的备选。
- 模型自动生成并执行下一轮 rework：失败报给 lead，lead 提交新 brief。
- 通用 gate DSL：固定 profile + 开关。
- 负载预测调度：静态 slots。
- 冻结全部文件字段：只冻结 identity、state、attempt、verification sha。
- 进 `radian` 二进制：先仓库外原型；坐实后新 ADR、改 ADR 0032/0043。

## 已验证的事实（2026-10-20，本机）

- `radian exec`：`-p -`、`--session-id`、`--resume`、`--workspace`、`--profile workspace`、`--yes`、
  `--output jsonl`（Result 含 outcome/turns/cost；cost 为会话累计）。缺省 profile 是 `full`（ADR 0043）。
- `radian attach`：只读、不开 socket、`--follow`；只渲染已提交消息，无流式 delta（ADR 0043 §7）。
- 控制口 `subscribe` 不含 delta（`crates/radian-host/CONTROL.md`）。
- ADR 0038：`workspace` 沙箱里 git 可写 refs、index、objects（能改写历史、删分支），只有 `config`、
  `config.worktree`、`hooks` 只读；linked worktree 的原生 commit 在沙箱里成功；工作区内 `core.hooksPath`
  目录可写且会执行。

## 未验证 / Spike 要碰的

- linked worktree 在 `workspace` 沙箱下跨 item 改 ref 的实际可行性与包装器检测是否全覆盖：这是**第一个 Spike**；独立 clone（`--reference` 共享 objects）作对照项。
- 包装器与模型同一 OS 用户时，沙箱是否真的遮住 push 凭据（ADR 0020）。
- jsonl 流在模型长思考时是否持续有行可用来判 stall。
- host-local attempt record 的字段冻结（token、pid + 启动时间、session、worktree 路径、branch、终止结果）。
- brief 里 `mutations:` 的声明格式。
- 原型语言与位置。

## 用户视角：跑起来的步骤

一次性：每台 host 装 radian、`radian login`、clone、ssh 免密；写 `arc.toml`；
`claude mcp add arc -- arc mcp --config arc.toml`。

循环：你说需求 → Claude 给计划（item、依赖、验收、mutations）→ 你点头 → Claude `add` → 你起 `arc run`
（或定时器）→ 想看就 `ssh <host> radian attach <item> --follow` → 进 `awaiting_human` 时 arc 打印
item、sha、verdict 路径 → 你 `arc pass <item> <sha>` 或 `fail` → fail 后 Claude 写 rework → pass 后下次
`run` 跑 Integration Gate → `ready_to_merge` 时你按今天的 ff-only 流程合并 → arc 看到 main 含 sha 就进
`done`，删 worktree。预算耗尽时 item 停在 `paused_budget`，你 `arc reset-budget <item>` 才继续。

日常五条：`arc run`、`arc list`、`arc pass/fail`、`arc reset-budget`、`radian attach`。

## 实施顺序（落地时）

| 阶段 | 范围 | 必过 |
|---|---|---|
| Spike 0 | 单 host：linked worktree 在 `workspace` 沙箱下的跨 item ref 风险与包装器检测（独立 clone 作对照）；固定 session id；包装器收 jsonl、分类、受控 push；对固定 sha 的独立 checkout 验证 | 改坏另一 item 的 ref 在该 item 结束时被包装器发现并 failed、不 push；无 Result 的崩溃判 terminated 不判 unknown；非 completed 不 push；push 时 hooksPath 置空生效 |
| Spike 1 | backlog 事务 + 两把锁 | 并发 `add/run/cancel` 不丢状态、不重复 claim；第二个 `run` 拿不到 `runner.lock` 即退；可重启 |
| Spike 2 | ssh + attempt + reconcile | ssh 断而远端仍跑时不重派；旧 attempt 结果不污染新状态 |
| Spike 3 | host 侧 push + 固定 sha 的 commands gate | 远端 ref 核验；gate 只认当前 sha |
| Spike 4 | 有界重试 + human gate + Integration Gate | 等人时 `run` 能退；同 sha 重审后旧 pass 作废；A 合入后 B 在 A+B 候选上复验；合并时 base 变了不合 |
| v0 试验 | 一个真实多 item、跨两 host 的需求 | 全程无 herdr；失败、返工、预算、合并可追溯 |

故障注入清单见审核报告第 10 节，照单做。

## v1 方向：desktop / iOS / web app

v0 的 backlog 文件格式（冻结的那几个字段）就是 API。v1 加 `arc serve`：持锁、loopback 或 ssh 转发上
的 HTTP/WebSocket（ADR 0013 不出网缺省不变），同一套 add/list/show/rework/cancel/pass/fail 加事件订
阅；`run` 与 `serve` 共用状态机。UI 展示 item 状态、host、attempt、session 观察流（arc 存的 jsonl，不
解析 attach 文本）、固定 sha 的 diff、验证证据、待审批动作。对手里 onorca.dev、OrchestratorInc、
Nimbalyst 都已带移动端，这是补齐不是领先。

## 修订记录（相对首版）

采纳：P0-1 短事务锁 + `state_version`；P0-2 `attempt_id` + `reconciling`，删"按 PID 接管"；P0-3
`done` = merged；P0-4 交付核验独立于 `Result.outcome`；P0-5 push 移到 host 侧包装器；P0-6 `--profile
workspace`；P1-1 身份三分（保留一 item 一 session）；P1-2 失败分类；P1-3 stall + 绝对上限；P1-4 可信命
令来源；P1-5/6 gate 隔离与 exclusive；P1-7 取消语义；P1-8 预算；P1-9 `run` 语义；P1-10 `retry_at` 绝
对时间；P1-11 TOML 与 ssh 字段；P1-12 无结束记录 = unknown；attach/jsonl 职责分开；费用按 attempt。
owner 保留：mutation 在 radian profile 里 required；不做 Sortie A/B；一 item 一 session 不变量。

第二轮审核（同日）采纳：P0-1 独立 clone 代替 linked worktree（ADR 0038 坐实）；P0-2
owner-only 在 v0 是流程保证不是安全边界；P0-3 只在 `completed` 且产物有效时 push，包装器 git 置空
hooksPath；P0-4 host-local attempt record（token、pid + 启动时间）作对账证据，worktree 上的 flock 作互斥；
P0-5 Integration Gate 对合并候选验证，合并时核 base；P1-1 预算耗尽进 `paused_budget` 不进
`awaiting_human`；P1-2 审批绑 `verdict_set_digest`；P1-3 `runner.lock` 与 `state.lock` 分开；收紧三条：包装
器确认退出即 `terminated` 不进 `reconciling`、独立 `AttemptFinished`、gate 只有固定 profile；P2：`supersedes`、
"只读"改"工作区可写、无交互入口"。owner 保持 idea 不开工，Spike 0 未启动。

对第二轮 P0-1 的保留决定：单机 v0 用 linked worktree 共享一个 clone，不做独立 clone；
风险改为包装器检测（head 是 base 后代、branch 与 HEAD 一致、push 后 ls-remote 核对）并安全失败，远端 ref
为真相；独立 clone 留作多 host 备选与 Spike 0 对照项。

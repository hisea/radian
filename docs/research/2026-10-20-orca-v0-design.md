# orca v0 设计稿（修订版，2026-10-20）

状态：**定为 v0，作为 idea 记录，不开工。** 本版按同日外部审核
（`2026-10-20-orca-v0-review-and-competitor-analysis.md`）修订：P0 六条全部采纳，P1 与文档矛盾
按本文"修订记录"一节处理。替代 herdr；不是 daemon。将来要 desktop / iOS / web app（见末节）。

## 角色

- **Claude（lead）**：分析需求、制定计划、拆成 item，经 orca 的 MCP 工具写入 backlog；看 gate 判定和
  diff，决定返工（提交新的 rework brief）；**不能**替 owner 批准。
- **orca（控制面）**：backlog 目录状态变更的**唯一写者**。接收 item、按依赖和容量派发、对账
  attempt、核验交付产物、起 gate、记录预算。确定性程序，不是模型。
- **worker**：一次性 `radian exec -p`，一个 item 一个 session、一个 worktree；跑完退出；只读，没有输入
  口（headless 无 prompter，`ask_user` 不挂载，ADR 0043 §4）；显式 `--profile workspace`。
- **gate**：可配置的阶段序列（commands / mutation / review / ci / human），每个阶段一次性运行、绑定
  一个 `verified_sha`，写判定。机械部分归它，判断归 Claude，批准与 merge 归 owner。
- **人类**：`radian attach <session-id> [--follow]` 只读观察已提交消息（ADR 0043 §7；无流式
  delta）；`orca show` 读 backlog；`orca pass/fail` 是 owner 权限。

## 三个身份

| 身份 | 规则 |
|---|---|
| `item_id` | backlog 文件名，永不变 |
| `session_id` | **= `item_id`**＼�一个 item 一个 session）。返工 `--resume` 回同一 host。`Result.outcome = poisoned` 等不可恢复时 item 进 `failed/irrecoverable`，人重新 `add`，不开第二个 session |
| `attempt_id` | `<item_id>-<n>`，每次执行一个。记 host、远端进程身份、worktree、branch、`base_sha`、`head_sha`、起止时间、session_id。旧 attempt 回来的结果因 token 不匹配被拒 |

## backlog：目录加 phase，单写者事务

```
<backlog>/
  lock                                  # 短事务锁，不是 run 整程锁
  todo/<item>.md                        # phase: ready | blocked | retry_wait(retry_at: <UTC>)
  doing/<item>.md                       # phase: starting | running | reconciling
  review/<item>.md                      # phase: verifying | rework_pending | awaiting_human | ready_to_merge
  done/<item>.md                        # phase: merged（done 的唯一定义）
  failed/<item>.md                      # phase: exhausted | cancelled | irrecoverable
  runs/<item>/attempts/<attempt-id>/
    execution.json                      # 上表 attempt 字段 + 结束方式
    result.jsonl                        # worker stdout 全流（ADR 0044 的 journal 行、delta 行、Result）
    stderr.log
    <stage>.verdict.json                # 每个 gate 阶段一份，含 verified_sha
```

- item 文件 = frontmatter（`schema_version`、`id`、`state`、`phase`、`state_version`、`after`、`host`、
  `execution{...}`、`verification{verified_sha, <stage>: passed|failed|skipped}`、
  `limits{max_attempts, max_cost, max_wall_time}`）+ brief 正文。v0 冻结的字段只有 identity、
  state/phase、attempt、verification sha；其余按 `schema_version` 演进。
- **单写者 = 任一状态变更事务只有一个持锁写入者**，不是"同一时刻只有一个 orca 进程"。事务：取锁 →
  核 `state_version` → 写临时文件 → fsync → 同文件系统 rename → 父目录 fsync → 释放。ssh、git、gate
  全在锁外。MCP server 与 `orca run` 因此可以共存。
- worker 和 gate 不写这个目录、不知道它的路径；brief 永远是 stdin 的一份拷贝。
- 不依赖 git 分支；要历史就让 orca 把目录当单分支仓库，只有它 commit。

## 状态转移

| 从 → 到 | 条件 |
|---|---|
| `todo.ready → doing.starting` | after 全部 `done.merged`，host 有空位；原子 claim，生成 attempt |
| `doing.running → review.verifying` | 收到**合法**的 attempt 结束记录（token 匹配、含 Result），且交付核验通过（见下） |
| `doing.running → todo.retry_wait` | 确认执行已终止且失败可重试；`retry_at` 持久化为绝对 UTC |
| `doing.running → doing.reconciling` | 远端状态未知（ssh 断、无结束记录）；**不自动重派**，等人 |
| `review.verifying → review.awaiting_human` | 所有 required 阶段对同一 `verified_sha` 通过 |
| `review.awaiting_human → review.ready_to_merge` | owner `pass <item> <sha>`，sha 必须等于 `verified_sha` |
| `review.ready_to_merge → done.merged` | orca 核 `origin/main` 包含 `head_sha` |
| `review.* → todo.ready` | lead 或 owner 提交 rework brief；attempt 编号 +1 |
| 任一 → `failed` | 重试或预算耗尽（`exhausted`）、cancel、不可恢复 |

分支在 gate 之后变动（`head_sha` 变）→ 该 sha 的 verdict 作废，回 `verifying`。`main` 在批准后变动
→ 合并前复跑 `commands` 阶段（集成复验）。

## 派发

v0 推（ssh + stdin），v1 拉（`radian work --pull`，需要 `orca serve`）；`radian work` 两种都接，它只收
一份 brief 加 item/attempt id。

### `radian work`（host 侧可信包装器，先是脚本）

1. 建或复用 `.radian-worktrees/<item>` 与分支 `work/<item>`（首轮自 `origin/main`，记 `base_sha`）。
2. 跑模型：
   ```
   radian exec -p - --session-id <item> --workspace <worktree> \
     --profile workspace --yes --output jsonl
   ```
   返工时加 `--resume <item>`。
3. 模型退出后，**包装器而不是模型**做：核 worktree 属于本 item、读 `head_sha`、列未提交和未跟踪文
   件、判断有无有效改动；然后 `git push origin work/<item>`（push 凭据只在包装器，模型的 shell 看不
   到）。
4. stdout 最后一行是 attempt 结束记录：Result 原文 + `attempt_id, base_sha, head_sha, dirty[], pushed`。
   没有这一行的 attempt 一律 `unknown`，进 `reconciling`，不接受部分 stdout。
5. 退出码透传（ADR 0044 §3）。

### orca 侧

- 收到结束记录后 `git ls-remote origin work/<item>` 核远端 ref == `head_sha`，再进 `review`。
- **交付核验独立于 `Result.outcome`**：`completed` 只表示执行链结束（ADR 0044），不是任务完成。
  dirty 非空、无有效改动（且 brief 未允许 no-op）、sha 不符，都按失败处理。
- **并发**：全局与每 host 静态上限；gate 的 `commands`/`mutation` 标 exclusive。
- **stall**：jsonl 流 `stall_timeout` 内无新行 → SIGTERM（退出码 143，outcome `cancelled`），**并**叠
  加 `max_wall_time` 绝对上限；两者都记失败类别。
- **失败分类**：基础设施（ssh、host）、模型、验证、取消。只有前两类进指数退避重试（10s 起，上限
  5min，`max_attempts` 内）；取消不重试；验证失败回给 lead 写 rework，orca 不自动生成下一轮 brief。
- **预算**：`max_attempts`、`max_cost`、`max_wall_time`；到顶进 hold（`awaiting_human`），不继续循环。
  费用按 attempt 增量算：`Result.cost` 是会话累计（ADR 0044），`run_*` 字段落地前按本次新增的
  journal 行算。
- **取消**：限定 attempt；SIGTERM → 宽限 → SIGKILL；等远端确认；确认不了进 `reconciling`。
- **重启**：不留内存状态。启动扫 `doing/`：每个 attempt 按远端进程身份对账，确认已停才按失败处理，
  确认不了进 `reconciling`。不按本地 ssh 子进程 pid 判断。
- **`orca run` 语义**：推进所有当前可做的转移，收束本次发起的工作，持久化，在没有立即可做的事时退出。
  `retry_wait`、`awaiting_human`、CI 等待都不堵它；下次由定时器或人再起。

### v1：拉

每 host 一个定时器跑 `radian work --pull <orca-url>`：向 orca 要一个 item；orca 作为唯一写者决定给不
给；给了就返回 brief 与 attempt id，脚本起新 session，跑完回传结束记录，退出。本机容量由脚本判断。

## gate：可配置的阶段序列

- 触发：item 进 `review.verifying`。每阶段一次性运行，绑定 `verified_sha`，产出自己的 verdict。
- 配置两级：orca 的缺省序列（profile）；brief 的 `gates:` 只能加严，不能删 `required`。
- **命令来源可信**：`commands` 阶段跑的是 profile 里的固定命令；brief 只能引用已允许的命令或申请例外，
  不能带任意 shell。

| 阶段 | 做什么 | 隔离 |
|---|---|---|
| `commands` | 对 `verified_sha` 的独立 checkout 跑验收命令（radian：Verify 七条，`--no-fail-fast`，易抖的重复 N 次），存命令、退出码、环境标识、尾部输出 | 独立 worktree；源码只读，构建产物与缓存有单独可写位置；exclusive |
| `mutation` | 按 brief 的 `mutations:`（行为 → 改坏方式 → 应红的测试二进制）逐个改坏、跑、恢复、比对 | 与 `commands` 不同树、不同时、不共享可变构建状态；exclusive |
| `review` | 只读 radian 会话读 diff、brief、Result、commands 结果；核对"保证"类注释；出 阻塞/建议/已核对 | 任意 host；不改代码；**不是** worker 同 session 的 self-review |
| `ci` | orca 推 PR，等 checks；视为 `commands` 的远端执行后端 | GitHub |
| `human` | owner 对 `verified_sha` + verdict 版本 pass/fail | 无；durable，`run` 不等它 |

- **缺省 profile**：通用 `commands(required) → human(required)`；**radian 仓库 profile**：
  `commands(required) → mutation(required) → review → human(required)`＼�AGENTS.md 今天就要求
  变异检查）。
- `on_fail`：`commands`/`mutation` 失败 → `rework_pending`（通知 lead，等它的 rework brief）；`review`
  阻塞 → `awaiting_human`。
- 判断归 Claude，批准归 owner：MCP 工具里没有 `pass/fail`。

## 配置（合法 TOML）

```toml
backlog = "~/.radian/orca/radian"
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
ssh_user = "hai"
path = "~/radian"
max = 2

[hosts.pc]
ssh_host = "<windows-host>"
ssh_user = "hai"
ssh_port = 2222
path = "~/radian"
max = 1

[profiles.radian]
gates = ["commands:required", "mutation:required", "review", "human:required"]
commands = ["cargo fmt --all -- --check", "cargo clippy --workspace --all-targets -- -D warnings", "..."]
```

ssh 参数分字段，argv 安全构造，不拼单段 shell 字符串。

## 不做（v0）

- 常驻进程：`orca run` 排空即退；worker/gate 一次性。
- worker 中途插话：纠正走 rework。
- worker 写 backlog、读 backlog 路径、持有 push 凭据。
- 跨机器 resume：返工固定原 host；失联保守等人。
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

## 未验证 / Spike 要碰的

- `workspace` 沙箱内 worktree 能否 commit（ADR 0038 的 `.git` 写边界；worktree 的 `.git` 文件指向主仓库）。
- jsonl 流在模型长思考时是否持续有行可用来判 stall。
- 远端进程身份用什么（session 文件锁、pid+starttime、包装器写的 token 文件）。
- brief 里 `mutations:` 的声明格式。
- 原型语言与位置。

## 用户视角：跑起来的步骤

一次性：每台 host 装 radian、`radian login`、clone、ssh 免密；写 `orca.toml`；
`claude mcp add orca -- orca mcp --config orca.toml`。

循环：你说需求 → Claude 给计划（item、依赖、验收、mutations）→ 你点头 → Claude `add` → 你起 `orca run`
（或定时器）→ 想看就 `ssh <host> radian attach <item> --follow` → 进 `awaiting_human` 时 orca 打印
item、sha、verdict 路径 → 你 `orca pass <item> <sha>` 或 `fail` → fail 后 Claude 写 rework → 你按今天的
ff-only 流程合并 → orca 看到 main 含 sha 就进 `done`，删 worktree。

日常四条：`orca run`、`orca list`、`orca pass/fail`、`radian attach`。

## 实施顺序（落地时）

| 阶段 | 范围 | 必过 |
|---|---|---|
| Spike 0 | 本地 jsonl adapter | 识别完整结束记录；无记录不算成功；记 commit/dirty |
| Spike 1 | backlog 事务 | 并发 `add/run/cancel` 不丢状态、不重复 claim、可重启 |
| Spike 2 | ssh + attempt + reconcile | ssh 断而远端仍跑时不重派；旧 attempt 结果不污染新状态 |
| Spike 3 | host 侧 push + 固定 sha 的 commands gate | 远端 ref 核验；gate 只认当前 sha |
| Spike 4 | 有界重试 + human gate + 合并安全 | 等人时 `run` 能退；main 更新后旧 verdict 不复用 |
| v0 试验 | 一个真实多 item、跨两 host 的需求 | 全程无 herdr；失败、返工、预算、合并可追溯 |

故障注入清单见审核报告第 10 节，照单做。

## v1 方向：desktop / iOS / web app

v0 的 backlog 文件格式（冻结的那几个字段）就是 API。v1 加 `orca serve`：持锁、loopback 或 ssh 转发上
的 HTTP/WebSocket（ADR 0013 不出网缺省不变），同一套 add/list/show/rework/cancel/pass/fail 加事件订
阅；`run` 与 `serve` 共用状态机。UI 展示 item 状态、host、attempt、session 观察流（orca 存的 jsonl，不
解析 attach 文本）、固定 sha 的 diff、验证证据、待审批动作。对手里 onorca.dev、OrchestratorInc、
Nimbalyst 都已带移动端，这是补齐不是领先。

## 修订记录（相对首版）

采纳：P0-1 短事务锁 + `state_version`；P0-2 `attempt_id` + `reconciling`，删"按 PID 接管"；P0-3
`done` = merged；P0-4 交付核验独立于 `Result.outcome`；P0-5 push 移到 host 侧包装器；P0-6 `--profile
workspace`；P1-1 身份三分（保留一 item 一 session）；P1-2 失败分类；P1-3 stall + 绝对上限；P1-4 可信命
令来源；P1-5/6 gate 隔离与 exclusive；P1-7 取消语义；P1-8 预算；P1-9 `run` 语义；P1-10 `retry_at` 绝
对时间；P1-11 TOML 与 ssh 字段；P1-12 无结束记录 = unknown；attach/jsonl 职责分开；费用按 attempt。
owner 保留：mutation 在 radian profile 里 required；不做 Sortie A/B；一 item 一 session 不变量。

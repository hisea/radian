# arc 的合并方式与钩子：可配置

- 状态：decided（可以派发；`arc-run-nonblocking.md` 已落地；`arc-serve.md` §4 的两个通知时间点并到这里一起做）
- 涉及 ADR：0048（§10 gate 与 integration、§11 批准与 `verdict_set_digest`、§13 调度循环、§15 配置与目录）
- 来源：arc 合并后远端留下 `work/<task>` 分支的讨论；`arc-flows.md`（每个仓库一套配置，不是 worker 种类）

## 背景

gate 已经是"种类由二进制定，内容由配置写"：`commands` 跑配置里的命令行，`review` 用配置里的模型路由和提示词，`human` 等人批准。merge 时的 integration 却几乎全部写死，而且是这个仓库自己的流程：把 task 合进当时的 main 生成合并提交（不 rebase），在候选提交上跑 `key_commands`，直接推 main，`done.merged` 后删本地 worktree、远端分支保留。

换一个仓库，合并方式常常不同：main 有保护只能走 PR；要 squash 或 rebase 保持线性历史；用 merge queue 或等 CI；目标不是 main。合并后的收尾也各不相同：删分支、发通知、更新看板。远端 `work/<task>` 分支今天没人删，公开仓库上越积越多。

## 决定（已定）

### 1. 不变式由 arc 保证，不能配置

不管用哪种合并方式：

- 合并前，远端 `work/<task>` 仍是 `verified_sha`，verdict 重算的 digest 等于记录的 `verdict_set_digest`（今天的检查不变）。
- 合并后，arc 自己核对目标分支上的结果就是验过的内容：sha 不变的方式核对目标分支等于候选提交；squash、rebase 这类会改 sha 的方式核对目标分支顶端的文件树等于候选提交的文件树。核对不过，task 不进 `done.merged`，原因写进 task。
- 冲突、`key_commands` 失败、合并方式报错：task 进 `review.rework_pending`，与今天相同。状态只由 arc 改。

### 2. 合并方式由配置选

`[integration]` 一节。`key_commands` 从 gate profile 移到这里，只此一处（owner 2026-10-29）：gate profile 里再写 `key_commands` 时配置加载失败，消息指明搬到 `[integration] key_commands`。attempt 记录的 `profile.json` 里旧的 `key_commands` 照常读出但不再使用，集成一律用当前 `[integration]` 的值。

- `method = "push"`（默认）：今天的做法。`style = "merge"`（默认，合并提交，不 rebase）、`"squash"`、`"rebase"`。在候选提交上跑 `key_commands`，无 force 推送，main 动了就重新集成（今天最多 3 次的规则不变）。
- `method = "command"`：运行 `[integration] command` 这一条命令，由它完成合并。环境变量与第 3 条钩子相同（另加 `ARC_CANDIDATE_SHA`）。stdout 最后一行是三种之一：
  - 一个 40 位 sha：合并完成，这是合并后目标分支的 sha；arc 拿它做第 1 条的核对。
  - `pending`：还没合并（比如在等人点 PR 或等 merge queue）。task 留在 `review.integrating`，下一次调度轮次再跑同一条命令；不计入"最多 3 次重新集成"。
  - `closed`：合并被放弃（PR 被关闭）。task 进 `review.rework_pending`，原因写进 task。
  - 退出码非 0、超时或最后一行不是这三种：等同集成失败，进 `rework_pending`。
- `method = "pr"`：`command` 的一个内置预设（owner 2026-10-29），不是另一套代码。选它等于 `method = "command"` 加一条内置的 `command`：用 `gh` 推 `work/<task>`、没有 PR 时开一个指向目标分支的 PR 并输出 `pending`、PR 已合并时输出合并后目标分支的 sha、PR 被关闭时输出 `closed`。预设命令全文写在手册里；想换平台（GitLab 等）就改用 `method = "command"` 自己写。谁点合并（人、merge queue、CI 自动合并）不归 arc 管。`method = "pr"` 时再写 `command` 配置加载失败。
- 目标分支：默认 `[repository] main_branch`，`[integration] target_branch` 可改。

以后的 gate 合批（`arc-gate-batch.md`）是另一种合并方式，不另开机制。

### 3. 钩子：固定时间点跑配置里的命令

```toml
[hooks]
after_merge = [
  "git push origin --delete work/$ARC_TASK",
  "git worktree remove --force $ARC_WORKTREE",
  "git branch -D work/$ARC_TASK",
]
```

- 时间点由二进制定义。第一批：`after_merge`（task 进入 `done.merged` 之后）、`after_fail`（进入 `failed.*` 之后）、`after_rework`（进入 `review.rework_pending` 之后）。
- `arc-serve.md` §4 的两个时间点也在这一批：`after_awaiting_human`（进入 `review.awaiting_human` 之后）、`after_proposed`（新 task 进入 `todo.proposed` 之后）。
- 上下文用环境变量传：`ARC_TASK`、`ARC_PLAN`、`ARC_ATTEMPT`（与 worker 的 `ARC_ATTEMPT` 同名同值，`<task>-<n>`；原稿的 `ARC_ATTEMPT_ID` 不用，免得一个值两个名字）、`ARC_BRANCH`、`ARC_WORKTREE`、`ARC_TARGET_BRANCH`、`ARC_MERGED_SHA`（只在 `after_merge`）、`ARC_STATE`（`<state>.<phase>`）、`ARC_HOOK`（时间点名）。
- 每个时间点是一个命令数组，按顺序跑；前一条失败后面照跑。
- 不丢、不重复执行的边界：进入某个状态的那次写入（`change_task`）同时在 task 记录里记一个待跑钩子；钩子由持有 `runner.lock` 的进程跑：调度轮次每一轮开头先跑完所有待跑钩子；CLI 的 owner 动作（`merge`、`reject`、`cancel`、`start`）结束时若能非阻塞拿到 `runner.lock` 就自己跑，拿不到就留给 `serve`。MCP 的 `add_task` 只留记录。一条钩子跑完（无论成败）才清掉记录，所以进程在钩子中途崩溃时下一轮会再跑一次：至少一次，手册写明钩子要能重复执行。
- 钩子在仓库根目录跑，超时用 `[gate] command_timeout_secs`，输出写进 task 的 run 目录。
- 钩子失败只记一行 `warning:`，不回退合并，不改 task 状态。
- 今天写死的"`done.merged` 后删本地 worktree"改为 `arc init` 写出的默认配置里的一行 `after_merge` 钩子；默认配置同时带上删远端和本地分支的两行。不想要的仓库删掉即可。

### 4. 本仓库的配置

`method = "push"`、`style = "merge"`，`key_commands` 不变，`after_merge` 用上面三行。行为与今天相同，只多了删分支。


## 验收

- 三种方式各有集成测试（假远端、假 `gh` 或假命令）：`push` 三种 style、`pr` 开 PR 后 `pending`、合并、关闭、`command` 输出 sha、输出错误 sha、`pending` 后下一轮完成。
- 第 1 条核对：`command` 输出的 sha 文件树与候选不同 → 不进 `done.merged`；squash 后文件树相同 → 通过。
- 钩子：环境变量都到达、失败只记警告、合并结果不受影响、五个时间点在对应转换后各跑一次（含 CLI owner 动作与 MCP `add_task` 引起的转换）；钩子中途进程被杀后下一轮补跑；`serve` 不在时 CLI 动作自己跑。
- gate profile 里写 `key_commands` 时加载失败且消息指向 `[integration]`。
- 默认配置删掉 worktree 那行后，`done.merged` 不再删 worktree（不再有写死的删除）。
- 手册 `docs/user/arc/index.md`：`[integration]` 与 `[hooks]` 每个键（类型、默认、全部取值、何时生效）、每个环境变量、merge 一节按新流程改写；配置 drift 测试看得到新键。

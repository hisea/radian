# arc 的合并方式与钩子：可配置

- 状态：decided（可以派发；排在 `arc-run-nonblocking.md` 落地之后，两边都改 integration 的代码）
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

`[integration]` 一节（`key_commands` 从 gate profile 移到这里，或 profile 里仍可覆盖，派发时定一处）：

- `method = "push"`（默认）：今天的做法。`style = "merge"`（默认，合并提交，不 rebase）、`"squash"`、`"rebase"`。在候选提交上跑 `key_commands`，无 force 推送，main 动了就重新集成（今天最多 3 次的规则不变）。
- `method = "pr"`：推 `work/<task>`，开一个 PR 指向目标分支，记下 PR，之后的轮次查它的状态；PR 被合并后做第 1 条的核对再 `done.merged`，PR 被关闭则进 `rework_pending`。谁点合并（人、merge queue、CI 自动合并）不归 arc 管。
- `method = "command"`：运行配置里的一条命令，由它完成合并，最后一行输出合并后目标分支的 sha；arc 拿这个 sha 做第 1 条的核对。退出码非 0 等同集成失败。
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
- 上下文用环境变量传：`ARC_TASK`、`ARC_PLAN`、`ARC_ATTEMPT_ID`、`ARC_BRANCH`、`ARC_WORKTREE`、`ARC_TARGET_BRANCH`、`ARC_MERGED_SHA`（只在 `after_merge`）、`ARC_STATE`。
- 钩子在仓库根目录跑，超时用 `[gate] command_timeout_secs`，输出写进 task 的 run 目录。
- 钩子失败只记一行 `warning:`，不回退合并，不改 task 状态。
- 今天写死的"`done.merged` 后删本地 worktree"改为 `arc init` 写出的默认配置里的一行 `after_merge` 钩子；默认配置同时带上删远端和本地分支的两行。不想要的仓库删掉即可。

### 4. 本仓库的配置

`method = "push"`、`style = "merge"`，`key_commands` 不变，`after_merge` 用上面三行。行为与今天相同，只多了删分支。

## 派发前要定的小事

- `key_commands` 放在 `[integration]` 还是留在 gate profile（一处为准，另一处报错）。
- `pr` 方式第一版只支持 GitHub（`gh`）还是从一开始就做成 `command` 的一个预设。

## 验收

- 三种方式各有集成测试（假远端、假 `gh` 或假命令）：`push` 三种 style、`pr` 合并与关闭、`command` 输出 sha 与输出错误 sha。
- 第 1 条核对：`command` 输出的 sha 文件树与候选不同 → 不进 `done.merged`；squash 后文件树相同 → 通过。
- 钩子：环境变量都到达、失败只记警告、合并结果不受影响、`after_fail` 与 `after_rework` 在对应转换后各跑一次。
- 默认配置删掉 worktree 那行后，`done.merged` 不再删 worktree（不再有写死的删除）。
- 手册 `docs/user/arc.md`：`[integration]` 与 `[hooks]` 每个键（类型、默认、全部取值、何时生效）、每个环境变量、merge 一节按新流程改写；配置 drift 测试看得到新键。

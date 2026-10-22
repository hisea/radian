# arc flows：一个仓库里按任务种类选 worker 配置与 gate

- 状态：decided（第 1、2 步）；idea（第 3 步）
- 涉及 ADR：0048（§10 gate profile、§14 v0 范围、§15 `config.toml`）、0043（单会话、`subagent` 子进程先例）、0044（`Result` 回传契约）、0025（skill）、0008（配置 schema）
- 来源：owner 问题 2026-10-22（一个项目同时跑 coding、game art、marketing content 等流程）

## 背景

arc 今天一个仓库只有一套流程，三个地方锁死：

- `gate.profile` 是全局单值。`gate.profiles.<name>` 已允许多个命名 profile，但整个仓库只能选一个。
- `worker.exec_args` 是全局的。所有 worker 拿同一套 `radian exec` 参数：同一个模型、同一个 `--mcp-config`、同一个 `--system-file`。
- ADR 0048 §14 写明 v0 只有 `radian exec` 一种 worker kind。

一个产品仓库里的工作并不都是代码：游戏美术资产、营销文案和代码任务需要不同的工具、不同的模型和不同的验证方式，但它们都适合同一套 plan / task / attempt / gate 状态机。

## 决定（已定）

一个 **flow** 是一个命名的配置捆绑：worker 怎么起、gate 用哪个 profile、交付的缺省怎么判。它**不是**新的 worker kind，也不是新的 agent 运行时：每个 flow 的 worker 仍然是 `radian exec`，wrapper（0048 §7）、delivery check（§8）、状态机（§6）、ledger 全部不变。只有 §14 那句"没有 worker kinds 之外的东西"改措辞：worker kind 仍只有一种，flow 只是参数。

### 第 1 步：`review` stage 落地

非代码 flow 的 gate 只有 `review` 和 `human` 有意义（`commands` 对美术和文案几乎无用）。`review` 在 0048 §10 已定（只读 radian 会话，不是 worker 会话），但今天列出它的 profile 会被拒。先把它建出来，否则 art / marketing flow 的 gate 只剩 `human`，等于没有自动验证。这一步独立于 flow，可以先派。

### 第 2 步：`[flows.<name>]` 表与 task 的 `flow` 字段

`config.toml` 加一张表，每个 flow 一节；`default` 节由 `init` 写出，承接今天的全局键：

```toml
[flows.default]
exec_args = []                    # 接替 worker.exec_args
gate_profile = "default"          # 接替 gate.profile

[flows.game-art]
exec_args = ["--model", "...", "--mcp-config", ".radian/arc/flows/game-art.mcp.json", "--system-file", ".radian/arc/flows/game-art.md"]
gate_profile = "art-review"       # review + human
allow_noop = false
```

- flow 节的键：`exec_args`（同今天的 `worker.exec_args`）、`gate_profile`（必须是一个 `gate.profiles.<name>`）、`allow_noop`（task 未指定时的缺省）。每个键按手册规矩写全名称、类型、缺省、允许值、何时生效。
- `worker.exec_args` 与 `gate.profile` 保留为 `flows.default` 的别名还是直接迁移（`schema_version` 升 2，按 0008 迁移），落地时定；两者只能留一个，不能两处都有值。
- task 文件加 `flow` 字段（字符串，缺省 `default`，必须是一个 `flows.<name>`）；`add_task` MCP 工具和 `radian arc add` 加同名参数。`show --json` 输出它。
- plan 层不知道 flow。一个 plan 可以混：coding task 出功能，`after` 它的 marketing task 写文案。
- 一个 task 的 flow 在创建后不变；返工 `--resume` 同一个 session，换 flow 等于换 worker 配置，应新建 task 并写 `supersedes`。
- flow 的工具差异靠 `radian exec` 已有的输入注入（`--model`、`--mcp-config`、`--system-file`、`--config`），不公开隐藏的 `--tools`，不加新输入。
- lead 的 brief 标准加一条：拆任务时写明 flow，选错 flow 的任务 gate 和工具全错。

## 待决（第 3 步：非代码交付物）

- 交付物仍必须是 git commit（0048 §8：`head_sha` 是 `base_sha` 后代、树干净）。文案没问题；美术二进制资产要么走 git LFS，要么 worker 只提交引用（路径、hash、生成参数），资产放外部存储。哪一种、以及 delivery check 是否要认 LFS 指针，未定。
- `review` stage 对非代码 flow 的评审标准从哪里来：profile 一段固定 prompt，还是 flow 节一个 `review_prompt_file`。未定。
- 是否需要每个 flow 自己的 `limits`（美术任务更贵、更慢）。未定，先用 task 级覆盖。

## 验收

- 一个仓库配两个 flow（`default` 带 `commands` gate，另一个带 `review` + `human`），同一个 plan 里各一个 task，`after` 串起来，`auto = merge` 全程无人到 `done`；两个 worker 的 `execution.json` 记录的 exec 参数不同，verdict 目录里各自只有自己 profile 的 stage。
- `flow` 指向不存在的节、`gate_profile` 指向不存在的 profile、`worker.exec_args` 与 `flows.default.exec_args` 同时有值，都在读 `config.toml` 时按名拒绝（exit 2），不起 worker。
- 没写 `flow` 的旧 task 文件按 `default` 跑，行为与今天完全相同。
- `docs/user/arc.md` 记录 `flows.<name>` 每个键与 task 的 `flow` 字段，drift test 读到允许值和缺省；0048 §14、§15 改成快照；删本文件。

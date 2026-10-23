# arc flows：一个仓库里按任务种类选 worker 配置与 gate

- 状态：已落地（第 1 步）；decided（第 2 步）；idea（第 3、4 步）
- 涉及 ADR：0048（§10 gate profile、§14 v0 范围、§15 `config.toml`）、0043（单会话、`subagent` 子进程先例）、0044（`Result` 回传契约）、0025（skill）、0008（配置 schema）
- 来源：owner 问题 2026-10-22（一个项目同时跑 coding、game art、marketing content 等流程）；mattpocock/skills PR #1316（software-factory 术语表：trigger、三道刹车、每个 task 的五个问题、ratchet / retro / fix 三个示例 task）

## 背景

arc 今天一个仓库只有一套流程，三个地方锁死：

- `gate.profile` 是全局单值。`gate.profiles.<name>` 已允许多个命名 profile，但整个仓库只能选一个。
- `worker.exec_args` 是全局的。所有 worker 拿同一套 `radian exec` 参数：同一个模型、同一个 `--mcp-config`、同一个 `--system-file`。
- ADR 0048 §14 写明 v0 只有 `radian exec` 一种 worker kind。

一个产品仓库里的工作并不都是代码：游戏美术资产、营销文案和代码任务需要不同的工具、不同的模型和不同的验证方式，但它们都适合同一套 plan / task / attempt / gate 状态机。

## 决定（已定）

一个 **flow** 是一个命名的配置捆绑：worker 怎么起、gate 用哪个 profile、交付的缺省怎么判。它**不是**新的 worker kind，也不是新的 agent 运行时：每个 flow 的 worker 仍然是 `radian exec`，wrapper（0048 §7）、delivery check（§8）、状态机（§6）、ledger 全部不变。只有 §14 那句"没有 worker kinds 之外的东西"改措辞：worker kind 仍只有一种，flow 只是参数。

### 第 1 步：`review` stage 落地（已落地）

`review` 已在 `BUILT_STAGES` 里；`[gate.review]` 的 `exec_args`、`prompt_file`、`timeout_secs` 已实现（ADR 0048 §10、§15），`docs/user/arc.md` 有手册条目。`prompt_file` 是 repository 级的评审标准；按 flow 的评审标准见第 2 步（profile 自带的 review 设置）。

非代码 flow 的 gate 只有 `review` 和 `human` 有意义（`commands` 对美术和文案几乎无用）。这一步独立于 flow，所以先落。

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
- 一个 `gate.profiles.<name>` 用 `reviewer = "<name>"` 选一个 reviewer profile（模型、prompt、时限、`fix_level`），见 `arc-reviewer-profiles.md`。这样不同 flow 的 review 可以用不同的模型和评审标准（一个只改文档的 flow 也可以用不含 `review` 的 profile）。
- `worker.exec_args` 与 `gate.profile` 保留为 `flows.default` 的别名还是直接迁移（`schema_version` 升 2，按 0008 迁移），落地时定；两者只能留一个，不能两处都有值。
- task 文件加 `flow` 字段（字符串，缺省 `default`，必须是一个 `flows.<name>`）；`add_task` MCP 工具和 `radian arc add` 加同名参数。`show --json` 输出它。
- plan 层不知道 flow。一个 plan 可以混：coding task 出功能，`after` 它的 marketing task 写文案。
- 一个 task 的 flow 在创建后不变；返工 `--resume` 同一个 session，换 flow 等于换 worker 配置，应新建 task 并写 `supersedes`。
- flow 的工具差异靠 `radian exec` 已有的输入注入（`--model`、`--mcp-config`、`--system-file`、`--config`），不公开隐藏的 `--tools`，不加新输入。
- 一个 `gate.profiles.<name>` 还有 `before_attempt`：attempt 开工前（worker 启动前）由 wrapper 在 worktree 里按顺序执行的步骤列表。内置步骤 `merge-main`（返工时把远端 main 合进 `work/<task>`，冲突则留在合并中、在 brief 顶部列出冲突文件交给 worker）由 PL0022 先做出来，缺省 `["merge-main"]`，`[]` 等于不做。本步加自定义命令 `{ run = "<命令>", timeout_secs = <秒> }`（例：预编译、拉 LFS、生成文件）：在 worktree 里跑，计入 attempt 的 wall clock，输出写进 attempt 目录，失败算基础设施失败（按重试规则，不派 worker）。是否在沙箱里跑、命令能否往 brief 前面加文字，落地时定。
- lead 的 brief 标准加一条：拆任务时写明 flow，选错 flow 的任务 gate 和工具全错。
- 每个 flow 节的注释（或手册里每个内置 flow 的条目）回答五个问题，照 software-factory 的模板：什么触发它；它读写什么（worktree、远端、外部服务）；完成后触发什么；人从哪里重新进入；什么阻止两个 attempt 干同一件事。后四个 arc 对所有 flow 的答案相同（`after` 与 `auto`；worktree 与 `work/<task>`；gate 与 `awaiting_human`；claim 事务与 token），所以 flow 定义只需写第一个，直到第 4 步把 trigger 做出来。

## 待决（第 3 步：非代码交付物）

- 交付物仍必须是 git commit（0048 §8：`head_sha` 是 `base_sha` 后代、树干净）。文案没问题；美术二进制资产要么走 git LFS，要么 worker 只提交引用（路径、hash、生成参数），资产放外部存储。哪一种、以及 delivery check 是否要认 LFS 指针，未定。
- 是否需要每个 flow 自己的 `limits`（美术任务更贵、更慢）。未定，先用 task 级覆盖。

## 待决（第 4 步：trigger，idea）

arc 今天只有两种启动：owner `start`，或 `auto` 级别在 `after` 全部合并后自动起。没有时钟，没有外部事件，没有 lead 之外的 ticket source。software-factory 的三个示例 task 只有加了 trigger 才做得出来，而且每个都是一个 flow（gate 为 `review` + `human`）：

- **ratchet**：每天一次，从 exception list 取一个文件修到过更严的检查，开一个小改动。
- **retro**：每合并 N 个 task 读一次 task、brief、verdict 与日志，给工厂自己的指令（AGENTS.md、brief 标准、skill）提改动。这是现有 dogfood 复盘规则的自动化。
- **fix**：arc 自己的 attempt 失败（基础设施类）时把日志立成 task；fix 的 attempt 失败不再链式触发；改工厂自己的改动永远要 `human`。

- trigger 种类最少两种：`clock`（间隔或每日时刻）与 `every_merges`（每合并 N 个 task）；trigger 属于 flow 还是属于 plan，未定。
- 节流与 trigger 一起定义，不另开配置：一个 trigger 刚加时按比例或频率节流，`max_auto_rework` 仍是每条循环的封顶。
- 触发产生的 task 由谁写 brief：固定 brief 模板跟 flow 走，还是仍由 lead 经 MCP 写。未定。
- 这一步需要常驻进程或外部时钟调用 `radian arc run`，与 0048 §14 "v0 没有常驻 daemon"冲突；落地时要先改 0048。
- per-flow 的 host / runner 字段（review 用便宜机器、implement 用沙箱机）属于这一步之后，0048 已把 host 字段留在文件格式里。

## 验收

- 一个仓库配两个 flow（`default` 带 `commands` gate，另一个带 `review` + `human`），同一个 plan 里各一个 task，`after` 串起来，`auto = merge` 全程无人到 `done`；两个 worker 的 `execution.json` 记录的 exec 参数不同，verdict 目录里各自只有自己 profile 的 stage。
- `flow` 指向不存在的节、`gate_profile` 指向不存在的 profile、`worker.exec_args` 与 `flows.default.exec_args` 同时有值，都在读 `config.toml` 时按名拒绝（exit 2），不起 worker。
- 没写 `flow` 的旧 task 文件按 `default` 跑，行为与今天完全相同。
- `docs/user/arc.md` 记录 `flows.<name>` 每个键与 task 的 `flow` 字段，drift test 读到允许值和缺省；0048 §14、§15 改成快照；删本文件。

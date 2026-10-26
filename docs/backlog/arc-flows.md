# arc flows：一个仓库里按任务种类选 worker 配置与 gate

- 状态：第 1、2、3 步已落地；第 4 步（trigger）idea
- 涉及 ADR：0048（§10 gate profile、§14 v0 范围、§15 `config.toml`）、0043（单会话、`subagent` 子进程先例）、0044（`Result` 回传契约）、0025（skill）、0008（配置 schema）
- 来源：owner 问题 2026-10-22（一个项目同时跑 coding、game art、marketing content 等流程）；mattpocock/skills PR #1316（software-factory 术语表：trigger、三道刹车、每个 task 的五个问题、ratchet / retro / fix 三个示例 task）

## 背景

flows 落地前，arc 一个仓库只跑一套流程：`gate.profile` 是全局单值，`worker.exec_args` 也是，所有 task 拿同一套 `radian exec` 参数和同一个 gate profile。一个产品仓库里的工作并不都是代码：游戏美术资产、营销文案和代码任务需要不同的工具、不同的模型和不同的验证方式，但它们适合同一套 plan / task / attempt / gate 状态机。

## 决定（已定）

一个 **flow** 是一个命名的配置捆绑：worker 怎么起、gate 用哪个 profile、并发上限、交付的缺省怎么判。它**不是**新的 worker kind，也不是新的 agent 运行时：每个 flow 的 worker 仍然是 `radian exec` 或一个自定义 command，wrapper（0048 §7）、delivery check（§8）、状态机（§6）、ledger 全部不变。只有 §14 那句"没有 worker kinds 之外的东西"改措辞：worker kind 仍只有一种，flow 只是参数。

flow 的落地范围（第 1 步 `review` stage、第 2 步 `[flows.<name>]` 与 task 的 `flow` 字段、第 3 步非代码交付物）已全部实现：每个 flow 有自己的 slots、worker、gate profile、`allow_noop` 缺省和 `before_attempt`，两个并发上限都生效，二进制交付物按普通 git commit 处理（不支持 Git LFS 与只提交引用的交付物）。手册在 `docs/user/arc/flows.md`，每个键在 `docs/user/arc/files.md`，TUI 显示 flow、等待原因与按 flow 的运行数。

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

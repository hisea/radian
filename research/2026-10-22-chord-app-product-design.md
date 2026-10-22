# chord 产品设计：三个界面、plan 层、全自动（2026-10-22）

名字：**chord**。弦是圆上两点的连线，和 radian 同属一个圆的几何；和弦是多个音同时奏响、要合拍，
就是编排多个 worker。此前的工作名 orca 与 onorca.dev 的产品同名，弃用；本目录下 2026-10-20 的三份
chord 文稿原名 orca，内容是同一个项目。

状态：产品设计，idea，不开工。本文在 `2026-10-20-chord-v0-design.md`（控制面）之上定产品形态：
用户看到什么、按什么、每个动作落到控制面的哪个转移。控制面的字段、锁、attempt 对账、gate 隔离、
push 规则不在本文重写，只引用。本文与 v0 设计稿冲突处以本文为准，冲突点在"对 v0 的改动"一节列全。

## 一句话

一个 app，三个界面：和 lead 头脑风暴产出 plan 与 task；浏览 plan / task 与依赖，点一下起一个无头
radian 跑一个 task，随时点开看它在做什么和改了什么；每个 task 一个合并界面，看 gate 结果和 diff，
合并或退回。plan 确定后可以开全自动，整个 plan 一直跑到完成。

## 对象只有四个

| 对象 | 是什么 | 谁建 | 谁改状态 |
|---|---|---|---|
| Plan | 一个产品需求：需求正文 + task 列表 + 依赖 | lead 经 MCP | app（confirm、auto、暂停）；状态 `done` 由 task 推出 |
| Task | v0 的 item，多一个 `plan` 字段 | lead 经 MCP | chord（v0 的目录 + phase 状态机） |
| Attempt | task 的一次执行（v0 不变） | chord | chord |
| Verdict | gate 一个阶段对一个 sha 的判定（v0 不变） | gate | 只增不改 |

Plan 字段：

```yaml
schema_version: 1
id: <plan-id>              # 永不变
title: ...
status: draft | confirmed  # done 不是存储状态：全部 task 都 merged 时界面显示 done
auto: off | run | merge    # 只有 confirmed 可以不为 off
limits: { max_cost, max_wall_time, max_auto_rework: 2 }   # plan 级预算，和 task 级叠加
tasks: [<task-id>, ...]    # 顺序只是显示顺序；依赖在 task 的 after 里
```

正文是需求本身：lead 和你头脑风暴的结论。它是 rework brief 的上下文来源，所以 confirmed 后正文只能
追加"修改 plan"记录，不能改写。

Task 相对 v0 item 只加一个字段 `plan: <plan-id>`。`after` 只能引用同一个 plan 的 task 或已 merged
的 task。Plan 不对应分支：merge 仍按 task 一个一个来，v0 的 `done = merged` 不变。

Plan 的存储和 task 一样是 backlog 目录下的文件：`plans/<plan-id>.md`，frontmatter 加正文。chord 仍是
唯一写者。

## 界面一：lead

- 一个嵌入的终端。缺省跑 Claude Code；也可以跑 radian TUI 作 lead。二者对 app 的接口相同：都是 MCP。
- app 起一个 MCP server（v0 的 `chord mcp`），lead 用它写 backlog：`add_plan`、`add_task`、
  `update_plan`（draft 阶段改正文与 task 列表；confirmed 后只能追加修改记录）、`add_rework`（给一个
  task 新 brief，attempt +1）、`list`、`show`。
- MCP 里**没有** `confirm`、`start`、`pass`、`fail`、`merge`、`auto`：这些是你在界面二、三的按钮。
  这是 v0 "批准不在 MCP 工具里"的延伸：lead 产出计划，不开工、不批准。
- 返工入口在界面三（退回），app 把 task、verdict、diff、你的评语送回这个 lead 会话，由 lead 写新 brief
  再 `add_rework`。送回的方式见"待决"。
- 头脑风暴阶段 plan 是 draft，lead 可以反复改。你在界面二点 confirm 才锁定。

## 界面二：plan / task

- 左：plan 列表，每个 plan 显示状态、`auto` 档位、task 进度（merged / 总数）、预算用量。
- 中：选中 plan 的 task 列表或依赖图。每个 task 显示 v0 的 `state.phase`、当前 attempt、花费。
- 每个 task 一个 **开始** 按钮：`after` 全部 merged 且 plan 已 confirmed 才可按；否则灰掉并写明在等
  哪个 task。按下即 v0 的 `todo.ready → doing.starting`，起 `radian work`，单机 v0 在本机。
- 点开一个 task：
  - 对话流：worker 的 jsonl 流渲染为已提交消息（v0 决定：UI 读 chord 存的 jsonl，不解析 attach 文本）。
  - 改动：三个时刻都给。跑的时候是 worktree 的实时 diff（含未提交）；结束后是 `base_sha..head_sha`，与
    verdict 绑的是同一个 sha；合并后 diff 不变，多一行"已在 main 的 `<merge sha>`"。
  - 摘要：改动旁边一段话说做了什么。来源是 worker 的 commit message（一个 task 多个 commit 时按序列
    出）；commit message 不够时再加模型摘要，见"待决"。
  - 多个 attempt 时按 attempt 切换，缺省最新。
- plan 级按钮：**confirm**（draft → confirmed，锁定 task 列表与依赖）、**auto** 三档开关、**暂停**。

## 界面三：合并

一个 task 进 `review.*` 后有这个界面。显示：

- `verified_sha`，以及这个 sha 对应的每个 gate 阶段的结果（commands、mutation、review、ci、integration），
  每个 ✅ 旁边写它对哪个 sha。分支动了（`head_sha` 变）✅ 自动消失，回 `verifying`：这是 v0 的
  "verdict 绑 sha"规则，界面上要让它可见。
- diff 与摘要（同界面二）。
- review 阶段的阻塞 / 建议 / 已核对三栏（有这个阶段时）。
- integration 结果：对当前 main 的 `base_sha`、`candidate_sha`、关键命令结果。

两个按钮：

- **合并**：v0 的 `pass` + integration + merge 合成一步。做的事不变：核 sha 等于 `verified_sha` 且
  `verdict_set_digest` 未变、跑 Integration Gate、合并时核 main 仍是 `base_sha`、ff-only、核
  `origin/main` 含 `head_sha`。任一不成立按钮失败并说明，不半做。
- **退回**：带你的评语。task 进 `rework_pending`，评语、verdict、diff 送回界面一的 lead，等 lead 的
  `add_rework`。chord 自己不写 brief（v0 决定不变）。

## 全自动

`auto` 是 plan 的字段，只有 confirmed 的 plan 可以打开。

| 档 | 做什么 | 停在哪 |
|---|---|---|
| `off` | 只有你按"开始"和"合并" | 每一步 |
| `run` | 依赖一 merged 就起下一个 task；gate 自动跑 | `awaiting_human`：你回来看一排 ✅ 逐个合并 |
| `merge` | `run` 之上，required gate 全过且 integration 过就合并，不等你 | 只在失败、预算、冲突时停 |

`merge` 档的审批者从你变成"gate 全绿"这个条件；v0 的审批绑 sha + `verdict_set_digest`、ff-only、
核 base 全部照旧。

失败回路：gate 失败 → app 把 task、verdict、diff 送回 lead → lead `add_rework` → 自动起下一个
attempt。同一个 task 自动返工最多 `max_auto_rework` 次（缺省 2），到顶**整个 plan 暂停**，不是只停这个
task；`auto` 回 `off`，等你。plan 级预算 `max_cost` / `max_wall_time` 到顶同样整个 plan 暂停，task 进 v0
的 `paused_budget`。暂停的 plan 要你显式 `reset-budget` 并重新打开 `auto`。

并发：依赖允许时可并行，每个 task 一个 worktree；gate 的 `commands` 与 `mutation` 是 exclusive，到
gate 时排队。单机 v0 缺省并发 1，先看清一个 plan 跑到底是什么样。

## app 即 runner

v0 定了"不常驻"：`chord run` 排空即退，谁来触发是待决。本文的回答：app 开着时持 `runner.lock`，就是
那个调度循环（`auto` 为 `run` / `merge` 的 plan 由它推进）；app 关了什么都不动；`chord run` 命令行留给
无头场景（定时器、ssh）。两者共用同一个状态机与文件格式。`off` 档的 plan 在 app 里也只响应按钮。

## 对 v0 设计稿的改动

| v0 | 本文 | 原因 |
|---|---|---|
| 只有 item，依赖是 `after` | 加 Plan 层；task 加 `plan` | 需求层给 rework 上下文；全自动的范围与开关要有载体 |
| `chord run` 自动 claim 全部 ready | 自动调度缩到 plan 级 `auto`，缺省 `off`，按钮起单个 task | 你是缺省的调度器；自动是显式打开的 |
| merge 由人在终端按 ff-only 流程做 | 界面三"合并"按钮做同一套动作；`auto: merge` 时条件满足自动做 | 有界面了 |
| 预算只有 task 级 | 加 plan 级预算与 `max_auto_rework`；到顶整个 plan 暂停 | 全自动必须有总闸 |
| ssh 多 host 推送 | v0 app 只跑本机；host 字段保留不实现 | 先单机 |
| 谁触发 `run` 待决 | app 开着即 runner | 化解"不常驻"与"要自动"的矛盾 |

不变：chord 单写者与两把锁、三个身份、attempt 对账、`radian work` 包装器与 push 规则、gate 固定 profile
与 exclusive、verdict 绑 sha、`done = merged`、chord 不自动写 brief、worker 无交互入口。

## 不做（v0 app）

- 多 host。
- 多种 agent 种类：worker 只有 `radian exec -p`。
- plan 分支或 plan 级 merge。
- 条件表达式、动态 DAG、gate DSL（v0 决定）。
- app 里直接给 worker 插话：纠正走退回 → lead → rework（v0 决定）。
- iOS / web：v1，见 v0 设计稿末节；本文的四个对象和 plan 文件格式就是它们要读的 API。

## 待决

- 界面技术已定，见下节"实现路径"。
- 退回与自动失败时怎么把上下文送回 lead：Claude Code 的会话从外部只能往 pty 写字；备选是 app 写一个
  `rework/<task>.md` 文件并在 pty 里送一行"读这个文件"的提示；radian 作 lead 时可走 inbox。
- 摘要来源：只用 commit message，还是在 gate 的 `review` 阶段顺带产出一段改动摘要。
- `plans/<plan-id>.md` 与 MCP 工具参数的字段冻结。
- lead 的 MCP 是否允许 `update_plan` 在 confirmed 后加 task（倾向不允许，走"修改 plan"记录 + 重新 confirm）。
- v0 设计稿"待决"一节的五项仍然有效（原型位置、Spike 0、`mutations:` 格式、attempt record 字段、`serve` 还是 `chord`）。

## 实现路径

三个界面读的全是 chord 的文件和 jsonl 流，UI 是薄客户端，可替换。顺序：

1. **CLI 先**：控制面本身，`chord add / list / show / start / merge / reject / auto / run`。这是另外两层共用
   的底座，也是 lead 的 MCP server 的实现。v0 设计稿的大部分工作量在这里，与 UI 无关。
2. **第一个交互界面是 TUI，独立二进制**，依赖 `radian-tui` 作库：直接复用 `attach`（只读 journal 视图，
   即界面二的对话流）、`blocks`（markdown、tool card）、`theme`、`keys`、层栈与事件循环（ADR 0023 的
   Foundation 层）。新写三块：plan / task 列表页、依赖图、整文件 diff 评审页（radian-tui 今天只有
   tool card 里的局部 edit diff；syntect 高亮可复用）。lead 在旁边的终端 pane 跑，不嵌终端。独立二进制
   带着 radian-tui 的整棵依赖树，两边版本要一致，原型期可接受；坐实后再并入 `radian chord`（ADR 0032
   一个二进制，三个界面成为 ADR 0023 的 workspace pages）。
3. **GUI 是 v1** 的桌面端，与 iOS / web 同列。gpui 的终端适配器与组件库版本互锁，先做 spike 再选。
   TUI 最先吃亏的是界面三的 diff 评审；到那时若受不了，可以只给界面三提前上 GUI。

## 验收（落地时）

- 一个真实需求在界面一产出一个 plan 与 ≥3 个带依赖的 task，confirm 后 `auto: off` 下逐个"开始"、
  逐个"合并"，全程没有 herdr，没有手敲 git。
- 同一个 plan 重跑一遍 `auto: run`：依赖满足即起下一个，全部停在 `awaiting_human`；再跑 `auto: merge`：
  全部进 `done`，中间没有人介入。
- 一个 task 的 gate 故意失败：`auto` 下自动走 lead 返工，第 `max_auto_rework + 1` 次失败时整个 plan 暂停、
  `auto` 回 `off`；`off` 下界面三"退回"把评语送到 lead，lead 的 `add_rework` 让 attempt +1 且 session 不变。
- 合并界面的 ✅ 在分支推新 commit 后消失；在 main 前进后"合并"失败并说明 base 变了。
- 界面二在 task 运行中能看到 worktree 实时 diff，结束后看到 `base_sha..head_sha` 与 commit message 摘要。
- 关掉 app，`auto: run` 的 plan 停止推进；再开，从文件恢复，不重派仍在跑的 attempt。

# 基础提示词：验收按任务指定的来；放松规则是用户的决定；工具描述瘦身

- 状态：decided＼�写成 backlog）；三部分可以分开落地
- 涉及 ADR：0036（基础提示词 `<role>` 段）、0043 §4（工具描述的字节上限）；另改 `AGENTS.md` 一句
- 来源：2026-10-20 A/B/C 测试 ab3（同一 brief、同一模型 Muse Contributor、同一台机器；radian / pi / opencode）

## 背景（已核实）

ab3 的结果：opencode 17 分钟、pi 约 21 分钟、radian 约 25 分钟（后两者已扣除磁盘满的约 7.5 分钟停顿）；第一次请求 opencode 24.9k / pi 5.5k / radian 9.7k token。三家都发现并堵上了 explore 子进程的 MCP 漏洞。radian 有两处行为另外两家没有：

1. **按项目文件而不是按任务跑验收**：brief 写"只跑改动涉及的 crate，完整 Verify 在 gate 做"，radian 照样跑了整个工作区的测试（这一轮多约 6 分钟；Mac 负载高时一次全量 30–40 分钟）。原因在基础提示词 "Verifying before you say it is done" 第一条：`Run the checks the project's instructions name (see <project_instructions>)`，没有任何一句让用户的任务说明优先。
2. **自己做了放松安全规则的决定**：radian 在实现配置解析时（会话 `18e054a6b1ec15a8-cd59`，19:55:32Z）主动把 `[mcp]` 子树从 ADR 0008 §3.2 的"明文密钥必须拒绝"里豁免，理由是类比 JSON 文件；随后用一个只改文档的 commit 把这条写进了 ADR 0008。brief 写的是"不清楚就自己选并写明理由"（lead 的写法有问题），但 pi 和 opencode 在同一句话下都保留了原规则。基础提示词只有泛泛的 "Ask the user only for a decision that is theirs to make"，没说哪些算；`AGENTS.md` 写着"要推翻一个决定，先用 docs-only commit 改 ADR"——这条是写给 lead 的流程，worker 读到的是同一份文件，照字面执行了。
3. **固定开销**：radian 的 10 个工具定义 14,129 B（约 4.1k token），第一次请求约 9.5–9.7k token，是 pi 的 1.8 倍。按大小：`shell` 2,380、`grep` 2,077、`patch` 1,906、`job` 1,800、`read` 1,784、`glob` 1,194、`subagent` 877、`skill` 795、`git_diff` 776（remove-git-diff 合入后消失）、`write` 529。

## 决定（同意写成 backlog；具体措辞落地时定）

- **验收顺序**：基础提示词的验收第一条改成：先跑用户这次任务指定的检查；任务没指定时按项目指令文件；都没有时跑能覆盖改动路径的最窄命令，并说明跑了什么。
- **决定的边界**：基础提示词加一条：放松安全、权限、沙箱或密钥相关的规则，或推翻项目文档（ADR、配置规则）里写明的决定，都是用户的决定。按更保守的那一种实现，把问题和建议写进回答里，不要自己改文档里的决定。`AGENTS.md` 加一句：只有 owner 和 lead 能改 ADR 里的决定，worker 发现需要改时只报告。lead 的 brief 模板里"不清楚就自己选"同时排除这两类。
- **工具描述瘦身**：在 ADR 0043 §4 的上限之内逐个审 `shell`、`grep`、`patch`、`job`、`read` 的描述与 schema 说明，去掉重复和可由 schema 表达的话；目标是第一次请求降到 pi 的 1.3 倍以内（约 7k token），同一套行为测试保持绿。

## 验收

- 提示词：系统提示词 golden/快照测试更新；一个回归测试（或固定 eval 任务）：brief 指定"只跑某 crate 的测试"时，radian 不跑全量；brief 里出现会放松 §3.2 的诱因时，radian 报告而不修改 ADR。
- 瘦身：抓第一次请求（本地 capture server，见 lead 2026-10-20 的测量方法）前后对比字节数；工具行为测试全绿。
- 落地后用两个新任务重跑一次 A/B/C，对照 ab3 的数据。

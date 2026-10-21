# `/context` 的网格视图

- 状态：decided＼�从三版原型里选了 B）
- 涉及 ADR：0035（字形、颜色只来自角色、thin）、0029（主题角色）、0015（C1 reported 与 estimated 分开）、0018（自动压缩阈值）
- 依赖：`tui-context-command.md`（数据层与纯文本版；按类别拆分的数据结构由它提供）
- 来源：拿 Claude Code 的 `/context` 截图问「能做成这样么」；lead 用 radian dark 调色板在终端里画了三版（A 照搬 ⛁ ⛶ ⛝，B radian 自己的字形，C 一条细彩条加表格），owner 选 B

## 决定（已定）

- **布局**：左边 10 行 × 20 列的网格，每格是上下文窗口的 0.5%；右边是图例。网格按类别顺序填格：各类别已用 → 空闲 → 压缩缓冲区（排在最后）。份额按最大余数取整，**有内容的类别至少占一格**，不会因为太小而消失。
- **字形**：已用 `◼`、空闲 `◻`（ADR 0035 已有的字形例外）、压缩缓冲区 `▨`。`▨` 要进 `check-glyph-coverage.py` 与 README 的字形表；ascii 档另配一套。
- **图例**（右边，自上而下）：模型名（加粗）、provider、`<已用估算> / <窗口> tokens (<百分比>)` 标 `estimated`、provider 上次报告的输入 token 标 `reported, last request`（ADR 0015 C1，没有时写 `none yet`）、空一行、斜体小标题 `estimated by category`，然后每个类别一行：色块 + `名称: <tokens> (<百分比>)`，最后是 `free` 与 `compaction buffer` 两行。
- **类别**：system prompt、context files、memory、tool definitions、skills、MCP tools、user messages、assistant text、tool results；reasoning 仅在 `count_reasoning` 打开时列出。
- **颜色**：每个类别一个主题角色，不写死颜色（ADR 0035）。缺省值取 radian dark/light 调色板里已有的色相（system prompt = muted、tool definitions = dim、context files = 金、memory = 橙红、skills = 黄、MCP = 绿、user = 紫（accent）、assistant = 蓝、tool results = 青；空闲 = frame 线色；缓冲区 = 黄）。主题文件没写这些角色时回退到这组缺省值。
- **网格下面的明细**：context files（每个文件一行：路径 · tokens）、MCP（server 数 · 工具数 · tokens）、skills（个数 · tokens）、memory（条数 · tokens），每段标题后弱一级写对应命令（`/mcp`、`/skills`、`/memory`）；最后一行写消息条数与本会话压缩次数；`/context all` 展开全部明细。
- **窄终端**：放不下网格加图例时，图例排到网格下面；仍放不下时退回纯文本版（`tui-context-command.md` 的输出）。
- 原型脚本：lead 会话 scratchpad 里的 `proto/context_proto_chosen_B.py`（颜色由 okhsl 按 Ottosson 公式换算，text/muted/dim 与 `theme.rs` 测试里的 RGB 一致）。

## 验收

- TestBackend 快照：120 列与 80 列各一张；`thin.rs` 风格的整屏快照里网格、图例、明细的位置与颜色角色。
- 单测：200 格的分配之和恰好 200；有内容的类别至少一格；缓冲区在最后；没有压缩阈值（或窗口未知）时不画缓冲区、并在图例说明。
- 字形覆盖检查包含 `▨`；ascii 档快照。
- 主题文件缺类别角色时用缺省值；写了就用写的。
- 落地时同分支改 ADR 0035（字形表与类别角色）与 0029（新角色），docs-only commit，并删掉本文件。

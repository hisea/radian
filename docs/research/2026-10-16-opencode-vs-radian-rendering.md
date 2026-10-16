# opencode / dsh 与 radian 的 transcript 渲染对照（2026-10-16）

来源与版本（全部按源码核实，除非标「未核实」）：

- **opencode** 2.0.30（anomalyco/opencode，commit f40dc43，`packages/tui/src/`），TUI 由 **OpenTUI v0.5.17** 绘制（`packages/tui/package.json` 走 catalog，根 `package.json:57` 锁 `0.5.17`；本文的 OpenTUI 行号按 tag `v0.5.17`）。
- **dsh-TUI**＠截图；源码 `ccch1mneyyy/dsh-TUI` 0.15.0，commit 6e9f07d，Ink/chalk 实现；本机装的 `@deepseek-ai/dsh` 0.2.0-rc.2 不含这套终端界面）。owner 的定位：**只作参考，radian 要有自己统一的一套，有自己的特色**。
- **radian**：origin/main 5836485（已含 ADR 0028、ADR 0035/5、ADR 0024）。「radian 今天」一列的渲染文本是**按代码复原**的，不是 TestBackend 的无头输出（时间所限，见文末「未做」）。
- **原型**：`docs/research/2026-10-16-render-prototype/render-prototype.sh`，在终端里打印「今天 → 提案」两段，每个有争议的点是一个开关（`--help`）。owner 的口味（2026-10-15 晚）：「简洁、干净、留白一致、有点文艺范」，所以原型默认是 **`--style rule`**（dsh 式：无底色、第一列标记、细线），`--style band` 是 opencode 式（底色卡片、`▌` 竖条）。

## 一、助手 Markdown

| 元素 | opencode（文件:行） | radian 今天（文件:行，复原文本） | 提案（原型开关） | 与已定 ADR 的冲突 |
|---|---|---|---|---|
| 标题 | tree-sitter 查询把 `#` 标记隐藏（OpenTUI `lib/tree-sitter/assets/markdown/highlights.scm`：`atx_h1_marker … @conceal (#set! conceal "")`）；样式 `markup.heading.N` = `markdown.heading` 加粗，一级再加下划线（`packages/theme/src/tui/syntax.ts:45-58`）；颜色 `$hue.accent.200`（`tui/src/theme/assets/v2/opencode.json` `base.markdown.heading`） | `#` 保留、角色 `md_heading_mark`（dim），文字 `md_heading` = TEXT 加粗，一级下划线（`blocks.rs:2735-2750`，`render.rs:2344-2346`，`theme.rs:686-687`）。复原：`  ## What I did` | 隐藏 `#`（`--marks off`）；文字用强调色加粗；层级：一级下划线，二级加粗强调色，三级起只加粗（dsh-TUI 的阶梯：H1 强调色加粗下划线、H2 次色加粗、H3 加粗、H4 加粗斜体、H5/H6 弱色，`markdown.ts:458-476`）。截图里标题上方那条短横线是 Markdown `---`，不是标题样式（`markdown.ts:49,438`：三个 `─`） | **反转 ADR 0035 决定 3 与修订 1 表「`#` 号保留」、ADR 0028 R2「保持合成的 `##`」**；要 0035 修订 6 |
| 加粗 | `markup.strong` = `markdown.strong` `#f5a742` 加粗（`syntax.ts:59`，暖橙） | `md_heading` 角色加 BOLD，即 TEXT 加粗（`render.rs:2352`） | 保持 TEXT 加粗（`--bold plain`）；`--bold warm` 是 opencode 的样子，只为对比 | 无 |
| 行内代码 | 反引号隐藏（`markdown_inline/highlights.scm` `code_span_delimiter @conceal`）；`markup.raw` = `markdown.code` 绿（`syntax.ts:63`） | 反引号不显示，`md_code` = YELLOW（`blocks.rs:2893`，`theme.rs:690`） | 不改（`--code text`）；`--code box`（加底色）只为对比，不推荐 | 无 |
| 有序列表 | 标记 `${start+index}.`，角色 `markup.list` = `markdown.listItem`（`$hue.interactive.200` 蓝）（OpenTUI `Markdown.ts:877-881`，`syntax.ts:61`） | `1. ` / `- `，`md_list_bullet` = VIOLET（`blocks.rs:2752-2830`，`theme.rs:694`） | 不改 | 无 |
| 表格 | `tableOptions={{style:"grid", cellPaddingX:1}}`（`routes/session/message-parts.tsx:188`）；grid = 外框 + 每两行之间横线 + 列间竖线（OpenTUI `TextTable.ts` 边框布局：`innerHorizontal` 在有边框且行数 >1 时为真）；**表头单元格用 `markup.heading` 样式**（`Markdown.ts:1390-1410`）；单元格内行内样式保留 | 全框线、每两行之间横线（ADR 0035）；表头无样式；**单元格把 piece 文本拼成纯字符串，行内样式丢失**（`blocks.rs:3047-3176`，`cell_text` 在 `:3055`）；框线角色 `md_hr` | 表头加粗强调色（`--header bold`）；单元格保留行内样式（代码、加粗） | 无（0028 R2 的实施细节） |
| 块间距 | 标题、列表、独立块前后 marginTop 1；段落之间只在源码有空行时空一行（`Markdown.ts:1318-1326`） | 块与块之间恰好 1 行空白（`blocks.rs:2347-2352`，ADR 0035 V1） | 不改 | 无 |
| 正文左边距 | `paddingLeft={3}`（`message-parts.tsx:181`） | 2 格标记列，助手为空白（`render.rs:197-201`） | 保持 2 格；回复的第一行在标记列放 `●`（dsh，`--marker dot`） | 0028 修订 1 规则 2 保持 2 格，`●` 是标记列内容，不冲突 |
| 代码块 | `<code>` 块，围栏隐藏（`highlights.scm` `fenced_code_block_delimiter … conceal`） | 围栏行显示、dim；与正文同列（ADR 0035 表） | rule 风格：顶线带语言 `┌─ rust ───`、左侧细线 `│`、短底线 `└──`（dsh-TUI 顶层代码块走 `CodeBlockFrame` 组件，截图可见；嵌套代码块只有 muted 的 ```` ```lang ```` 开栏行、2 格缩进、**没有闭栏**，`src/terminal-utils/markdown.ts:341-349`） | **反转 0035 修订 1 表「显示围栏行」**；进 0035 修订 6 |

## 二、展开的 thinking

| 元素 | opencode | radian 今天 | 提案 | 冲突 |
|---|---|---|---|---|
| 头部 | `ReasoningHeader`（`message-parts.tsx:120-160`）：`Thought`，有标题时 `: 标题`，完成后 ` · 时长`（`util/locale.ts:28-45`：`<1s` 用 ms，`<60s` 一位小数 `3.2s`，之后 `Nm Ns`）；颜色 `text.feedback.warning`（`#f5a742`），展开时 alpha 0.6、折叠时全色；只有 hide 模式下才有 `- `/`+ ` 折叠符 | 无头部行；折叠占位 `thinking (N lines)`（`blocks.rs:1921-1930`），`▸` 在标记列（ADR 0028 规则 5）；**没有时长**，projection 不记 reasoning 起止 | 展开：`▾ thinking · 3.2s`；折叠：`▸ thinking · 3.2s · 8 lines`；颜色用 `reasoning` 角色（不用暖色，保持单色） | 改 ADR 0028 R3 的占位格式；需要 projection 记 reasoning 的开始/结束时刻（ADR 0010 live 事件已有时间戳：未核实字段名） |
| 标题行 | 正文第一行若是 `**粗体**`，折叠时当标题（`context/thinking.ts:9-14`） | 无 | 可选：折叠时显示第一行粗体作标题；原型未做 | 无 |
| 正文 | `<code filetype="markdown">`，`generateThinkingSyntax` 把每个 token 的 fg 都换成 `text.muted`（`thinking-syntax.ts`）：保留粗体/斜体与结构，去掉颜色；左边 `┃`（`ui/border.ts:15-21` `SplitBorder`），颜色 `theme.decrease(background.base)`；头部与正文之间 `marginTop={1}`，正文 `paddingLeft 1` | `~ ` 标记（`render.rs:201`）+ 纯文本 `prose_lines`（`blocks.rs:1341`），不做 Markdown，整体斜体 THINKING 色（`theme.rs:665`）。复原：`~ **Checking…**`（`**` 原样露出） | Markdown 结构、单色（`reasoning` 角色）、不斜体；左侧细线 `│`（rule）或 `▌`（band），dim（`--rail on`） | 改 0028 R3（斜体、纯文本）；`~ ` 标记作废 |
| 与下块间距 | `marginTop 1` | 1 行空白（ADR 0035） | 不改 | 无 |

## 三、Edit / patch 的 diff

| 元素 | opencode | radian 今天 | 提案 | 冲突 |
|---|---|---|---|---|
| 容器 | `BlockTool`（`routes/session/index.tsx:2726-2800`）：左边 `┃` 用终端底色、背景 `background.raised`、上下各 1 行、左 2 格、`gap 1`；标签 `← Edit` + 路径，muted（`:3297`） | ADR 0028：无方框、中性底色、半格留白、head `✓ patch 路径`、末行 `⟦0.03s⟧` | rule 风格：**无底色**，一行 `• patch 路径 · 0.03s`（工具名蓝，dsh 的 `Bash(...)`），正文在左侧细线 `│` 下；band 风格：保持修订 4 的卡片 | **rule 风格反转 0028 修订 4 决定 1-4（底色、半格留白）**；要 0028 修订 5 |
| 行号 gutter | OpenTUI `Diff.ts:530-586`（unified）：一列行号，`+` 与上下文行用新文件行号，`-` 用旧文件行号；行号后 ` +`/` -`；`showLineNumbers` 默认 true（`:132`） | 无行号；`+`/`-` 在行首，角色 `diff_added`/`diff_removed` 前景色（`tools.rs:320-343`，`:611`；`theme.rs:726-729`） | 加行号 gutter（`--numbers on`），格式 `  120 + ` | 无 ADR 冲突；**前提**：`patch` 的结果文本要带起始行号。`tools.rs:607-609` 说 patch 的条目文本是「合成的 `-old/+new`」，是否带 `@@` 行**未核实**（`radian-coding/src/patch.rs` 未读）；`git_diff` 本来就有 `@@` |
| 行底色 | `+` 行 `diff.background.added #20303b`、`-` 行 `removed #37222c`、上下文 `context`（= raised）；gutter 另一组更深的底色（`lineNumber.background.added #1b2b34` / `removed #2d1f26`）；符号色 `highlight.added #b8db87` / `removed #e26a75`（`opencode.json` `base.diff`，`index.tsx:3306-3315` 传入） | 只有前景色 | rule 风格默认**关**（只前景色，和 dsh 一样干净）；band 风格开（`--tint`） | 开的话要新角色 `diff_added_bg`/`diff_removed_bg`（ADR 0029）；pi 主题文件有没有对应键**未核实** |
| 语法高亮 | diff 正文按路径后缀高亮（`index.tsx:3302-3304` `filetype(path())`） | 无 | 复用 `highlight.rs`，按路径后缀（`--numbers on` 时原型里是粗糙的示意） | 无 |
| hunk 头 | 第一个 hunk 的 `@@` 不显示，之后的 hunk 之间显示一条 `hunkHeader` 色的横条（`component/patch-diff.tsx:117-125`） | 每个 `@@` 行显示，`diff_hunk` = VIOLET | 保留显示，改 dim（结构信息，不该抢眼） | 无 |
| 宽屏 | 宽度 >120 自动 split 视图（`index.tsx:3283-3288`） | 无 | 不做 | 无 |

## 四、dsh-TUI（截图 + 源码 `ccch1mneyyy/dsh-TUI` 6e9f07d）

- 布局自述（`src/components/MessageList.tsx:38-44`）：用户提示在灰色气泡上带 `❯` 指针；助手文本带 `●` 圆点加 Markdown；thinking 流式时是三行/全文切换，结束后是一行 `⚓ Thinking` 加本地化的 ctrl+o 展开提示；工具调用是「状态点卡片」（截图里是 `• Bash(参数) · 0s`，输出在下方对齐）。
- 标题无 `#`，六级阶梯（`src/terminal-utils/markdown.ts:458-476`，见上表）；`---` 是三个 `─` 的短线（`:49`、`:438`）。截图里标题上方的短线就是它。
- 代码块：顶层走 `CodeBlockFrame`（截图里的顶线带语言、左侧细竖线）；嵌套的只有 muted 开栏行、无闭栏（`:341-349`）。
- 对 radian 的意义：它证明「一种线宽、一种灰、同一列标记、不靠底色」能把块分清楚，和 owner 要的「简洁、干净、留白一致、文艺」一致。**不照抄**：radian 的标记列、强调色（VIOLET）、半格留白的输入框是自己的；下面的决定按「radian 自己的一套」来定。

## 五、给 owner 的决定（每条一个原型开关；推荐在前）

1. **整体风格：`rule`（细线、无底色）还是 `band`（底色卡片）。** 推荐 `rule`。代价：ADR 0028（昨天刚定的中性底色卡片、半格留白）要改成修订 5：工具卡片 = 一行 head + 左侧细线正文；用户消息 = `❯ 文本`（强调色，无带）。bar 输入框的 `▌` 与半格留白（0035 修订 3/4）是否也改成细线，一并定；不定就只有输入框带底色，和 dsh 截图一致。
2. **标题隐藏 `#`。** 推荐隐藏：一级强调色加粗下划线、二级强调色加粗、三级起只加粗（三档，不照 dsh 做六档）。反转 0035 决定 3 → 0035 修订 6。
3. **回复第一行 `●`。** 推荐开；标记列 2 格不变。
4. **加粗颜色。** 推荐不上色（TEXT 加粗）。
5. **行内代码。** 不改（YELLOW，无反引号）。
6. **表头加粗强调色 + 单元格保留行内样式。** 推荐做；是 0028 R2 的实施缺口（`cell_text` 丢样式），不是设计改动。
7. **thinking 头部带时长**：`▾ thinking · 3.2s` / `▸ thinking · 3.2s · 8 lines`。推荐做；需要 projection 记时刻。0028 R3 修订。
8. **thinking 正文**：Markdown 结构、单色、不斜体、左侧 `│`。推荐做；0028 R3 修订（去掉斜体与纯文本）。
9. **diff 行号 gutter。** 推荐做；先核实 `patch` 结果是否带行号，不带就要 `radian-coding` 在结果里给起始行（0028 R4 的工具卡片数据）。
10. **diff 行底色。** `rule` 风格推荐关；若 owner 要，新角色 `diff_added_bg`/`diff_removed_bg` 进 0029。
11. **diff 语法高亮。** 推荐做，复用 `highlight.rs`。
12. **`@@` 行改 dim。** 推荐做。
13. **代码块改线框**（顶线带语言、左细线、短底线），取代 dim 围栏行。推荐做；0035 修订 6。

14. **主题＼�「不要求完全 compatible，我们可以自己一个格式，但是可以有一个 skill translate」；「pi theme 里面有我们不需要的可以去掉」）。** 推荐：radian 自己的主题格式，角色集只保留这套设计用到的，pi 主题用一个内建 skill 翻译成 radian 格式（翻译是一次性的文件转换，不是运行时兼容层）。这反转 ADR 0029 决定 1「格式就是 pi 的主题 JSON」，要 0029 修订 2。这套设计需要而今天没有的角色：`frame`（细线、开口框、表格线、压缩线，今天借 `md_hr`）、`tool_name`（工具行的工具名）、`diff_line_number`、`diff_added_bg`/`diff_removed_bg`（只在开底色时用）、`status_ok`/`status_warn`/`status_error`（工具行的 `✓`/`✗` 与面板的 `● ◐ ✓ ◌ ✗`）；要改的映射：`user` 从背景角色改成前景角色（`❯` 与用户文字用强调色），`diff_hunk` 从 accent 改 dim；`reasoning` 去掉 italic 是内建取值的改动。可以去掉的 pi 键（这套设计不用）：`customMessage*`、`bashMode`、`mdCodeBlockBorder`、`borderMuted`、`selectedBg` 之外的 `*Bg` 若不开底色、`thinkingOff…Max` 七档若 footer 不按档位上色。降级：设计不靠底色，0028 修订 4 第 6 条「无背景色退回方框」可作废；不做 16 色一档，`COLORTERM` 不满足就走 256 色近似（已有）。原型：`render-screen.sh --theme dark|light|<pi 主题名>`，内建 light 的值来自 `theme.rs` `mod pi` 的 light 常量，19 个 pi 主题来自测试数据（`themes.py` 按 `theme/file.rs` 的映射取键）。

ADR 归属：2、13 与风格 1 的「线 vs 带」进 **0035**（视觉语言）；1 的卡片形状、6、7、8、9、11、12 进 **0028**（内容渲染：R2/R3/R4 与修订 5）；10 与 14 的角色和格式进 **0029**（修订 2）。

## 未做 / 未核实

- 没有用 TestBackend 无头渲染 radian 的真实输出；「今天」一列按代码复原，原型的 `--today` 同理。实施前先补快照。
- `patch` 结果文本是否带行号（`radian-coding/src/patch.rs`）。
- pi 主题 JSON 有没有 diff 底色的键（0029 兼容）。
- reasoning 起止时刻在 projection / live 事件里的字段。
- dsh-TUI 的 `CodeBlockFrame` 组件与工具卡片组件的具体字符（只看了截图与 `MessageList.tsx` 的自述）。

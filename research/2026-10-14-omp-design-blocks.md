# omp 的 transcript 块、输入框、状态、footer、面板、窄终端 —— 调研

日期：2026-10-14 · 调研者：agent · 状态：只读调研

本文是 `research/2026-10-14-omp-design.md` 的分篇 2，覆盖 **§3 transcript 每种块**、**§4 输入框（editor/composer）**、**§5 状态与反馈**、**§6 footer / 状态栏**、**§7 选择器与面板**、**§8 窄终端与 resize**，以及 **§8.5 与 Radian 的逐块对照表**。出处写法见主篇 §0。

**与 `research/2026-10-14-omp-blocks.md` 的分工**：那一篇已覆盖「块的种类与外观」的总原则、调用/结果配对、折叠与预览、`!` 命令。本文**不重复**那些结论，只在需要时引用（写作「见 blocks 篇 §x」），并**补上**它没覆盖的：每种块的**具体字形与角色**、输入框、状态反馈、footer、面板、窄终端。

---

## 3. transcript 的每种块

### 3.1 用户消息

- **组件**：`tui/src/chat/user-message.ts`（537 行）。内部是一个 `Markdown(text, 1, 1, getMarkdownTheme(), { bgColor, color })`（`:174-179`）——**`paddingX=1`、`paddingY=1`**，带 `userMessageBg` 背景。
- **无左侧标记**：靠**整块背景**区分（blocks 篇 §1.2）。首行左上角可放一个 `*`（accent 色）表示 live-steer（`user-message.ts:299-302`，`#badgeRow`）。
- **首行的 OSC 133 标记**：`OSC133_ZONE_START = "\x1b]133;A\x07"`（`:53`）加在首行前、`OSC133_ZONE_CLOSE`（`:57`，= `133;B` + `133;C` + `133;D;0`）加在末行后（`:350-352`），给终端多路复用器识别 prompt 区。注释解释了为什么 `133;C` 必须紧跟 `133;D;0`（`:39-52`）。
- **图片**：`base64ImageNode` 的缩略图（`:246-262`），走 Kitty 协议。
- **上下的留白**：`paddingY=1` 的两行**带背景**（`#renderEmptyPaddingLines` 在 `bgFn` 存在时 `applyBackgroundToLine`，`components/markdown.ts:3216`），所以容器**不会**剥掉它们（`isPlainBlank` 判据是「不含非空白」，带背景的行含 ANSI → 不算空白，`transcript-container.ts:164-166`）。这就是 omp 里「用户块上下有空白」的实现（blocks 篇 §1.1）。
- **原生路径**：`describe()`（`:211-285`）画一个 `card({ role: "omp.user", tone: "user" })`，含 hover 工具条（Copy/Rewind/时间，`:216-256`）、图片行、`md(text)`、badge（`:268-285`）。

### 3.2 助手正文

- **组件**：`tui/src/chat/assistant-message.ts`（1945 行）。正文是 `#createMarkdown("text", trimmed)` → `new Markdown(text, 1, 0, proseTheme, proseStyle, 0)`（`:1339`）——**`paddingX=1`、`paddingY=0`、`codeBlockIndent=0`**。
- `proseStyle` 来自 `#getProseTextStyle()`（`:414-418`）：只有主题有 `assistantMessageText` 角色时才给一个 `{ color, proseFg: true }`——`proseFg` 的语义是「标题/链接这类**语义化**的 span 保留自己的 `md*` 色，只有普通文字用这个色」（`components/markdown.ts:1320-1327` 的 `DefaultTextStyle.proseFg` 注释）。
- **一个助手消息切成多个块**：`updateContent` 遍历 `message.content`（`:1838-1885`），`text` 类型每段一个 `Markdown`，`thinking` 类型每段一个（见下），中间夹在 toolCall 之间的文本会切成独立的助手块（blocks 篇 §2.2）。
- **段间留白**：thinking 块后面若还有可见内容，加一个 `new Spacer(1)`（`:1874-1878`，注释「Add spacing only when another visible assistant content block follows」）。正文段之间**不加**（`Markdown` 自己的块级间距负责，见分篇 1 §2.3）。
- **错误块**：`#appendErrorBlock`（`:1494-1510`）→ `WidthAwareText`，`formatErrorBlock(message, contentWidth, maxRows, (line, i) => theme.fg("error", i === 0 ? \`Error: ${line}\` : line))`，`maxRows = #errorExpanded ? ∞ : MAX_TRANSCRIPT_ERROR_ROWS = 8`（`:47`）。前置一个 `Spacer(1)`（`:1916`）。
- **aborted**：`message.stopReason === "aborted"` 时用 `theme.fg("error", text)` 加一个 `Spacer(1)`（`:1901-1906`）。
- **恢复型错误**（`compact-recovered`）：dim 一行（`:1900-1902`）。
- **rewound 标记**：一个 `badge({ text: "↺ rewound", tone: "muted" })`（`:991-999`）。
- **turn usage 行**：`formatTurnUsage(summary)`（`overlays/usage-row.ts:97-105`）→ `` `${time} · ${tokens} tok · ${cost}` this turn ``，dim，前面可能有 Copy 图标与 `·`（`assistant-message.ts:1000-1030`）。

### 3.3 thinking

- **正文**：`#createMarkdown("thinking", text)` → `new Markdown(text, 1, 0, getMarkdownTheme(), { color: theme.fg("thinkingText", value), italic: true })`（`assistant-message.ts:1340-1343`）——**`paddingX=1`、`paddingY=0`、斜体、`thinkingText` 色**。
- **折叠头**（18.10.4 新增）：
  - 流式中：一个 `row` 含 `spinner(style: "starburst")` + `"Thinking…"`（muted）+ `elapsed(now - start)` + 可选 `rate(value, unit: "tok/s")`（`:931-943`）。
  - 结束后：`thoughtLabel(clock)`（`:51-55`）：`clock.end === undefined` → `"Thought"`；否则 `ms < 60_000 ? \`Thought for ${max(1, round(ms/1000))}s\` : \`Thought for ${formatDuration(ms)}\``（`:52-55`）。
  - `title` 属性带 token 数：`tokens > 0 ? \`${formatNumber(tokens)} tokens\` : undefined`（`:928-929`）。
- **折叠状态**：`node("section", { role: thinkingLive ? "omp.thinking.live" : "omp.thinking", collapsible: true, collapsed: this.#thinkingCollapsed.get(index) ?? (this.#hideThinkingBlock || (!thinkingLive && !this.#expandThinkingBlocks)), took: … })`（`:945-962`）。即：**默认流式中展开、结束后折叠**；`Ctrl+T`（`hideThinkingBlock`）时只留流式的头（`:926-927`）。
- **`thinking` 级别的 7 个符号**：`symbols.ts:191` 的 `thinking.minimal/low/medium/high/xhigh/max/autoPending` = `"○ min"`…`"◉ max"`、`"⟳"`（`symbols.ts:565-571`）。**注意**：这些是**footer 的 thinking 级别标签**（带文字），不是 thinking 块的图标。
- **ANSI 路径的 `※` 脉冲**：`THINKING_DOTS_FRAMES = ["✻","✼","❉","❊","✺","✹","✸","✶"]`（`:150`），`#thinkingDotsLabel()`（`:682-698`）输出 `<glyph> Thinking`（thinkingText + muted）+ 可选 ` · <tokens> · <rate> toks/s`。这是**旧路径**（原生路径用上面的 starburst）。
- **`proseOnlyThinking`**：`resolveThinkingDisplay`（`:127-137`）可以把非散文的 thinking 过滤掉（默认 `true`，`#proseOnlyThinking = true`，`:526`）。

### 3.4 每种工具的卡片

**共同的拓扑**（`tui/src/render/tool-card.ts`，424 行；blocks 篇 §4.2 已讲生命周期，这里讲外观）：

- `ToolCardVariant = "framed" | "plain"`（`:27`）：`framed` 自己画圆角边框（`CachedOutputBlock` → `renderOutputBlock`）；`plain` 内联流动（`Text` + 可选状态背景）。
- `ToolCardPhase = "pending" | "running" | "partial" | "success" | "warning" | "error" | "info"`（`:24`），映射到输出框的视觉状态 `toolCardState`（`:81-85`：`partial → pending`、`info → undefined`、其余同名）。
- **边框色**（`render/output-block.ts:82-90`）：`error → "error"`、`warning → "warning"`、`running`/`pending → "accent"`、其余 → `"dim"`；可被 `borderColor` 覆盖。
- **背景**（`:92-105`）：按状态取 `toolPendingBg`/`toolSuccessBg`/`toolErrorBg`（`render/utils.ts` 的 `getStateBgColor`），并用 `bgFill` 语义（`\x1b[0m`/`\x1b[49m` 之后重开背景，防止内层重置打出洞）。
- **header 一行**：`renderStatusLine(options, theme)`（`render/status-line.ts:33-55`）：`图标 + " " + 标题`，描述用 `": "`，badge 用 `⟦…⟧`，meta 用 `theme.sep.dot`（`" · "`）。**多行字段会被压平**（`flattenForHeader` `:27-30`，把 CR/LF 换成空格），因为 header 必须是一行。
- **内容内边距**：`outputBlockContentWidth(width, left, right) = max(1, width - 2 - left - right)`（`render/output-block.ts:64-72`），`normalizeContentPaddingLeft` 默认 **1**（`:49-52`）。`cap = horizontal.repeat(3)`（`:79`）——**顶框标签前的 3 格 `─`**（`╭─── <label> ───╮` 的形状）。
- **section 分隔线**：带 `label` 的 section 画 `├─── label ───┤`（`teeRight`/`teeLeft`，`:130-138`）；无 label 但 `separator: true` 且不是第一个时画纯分隔线（`:139-145`）。

#### bash（模型工具）

- **head**（`tools/bash.ts:501-512`）：成功时 `iconOverride = theme.styledSymbol("tool.bash", "accent")`（`❯`）+ `title = resolveTitle(args, options)`；非成功时 `icon: isPartial ? "pending" : isTimeout ? "warning" : "error"`。`resolveTitle` 默认 `args?.name ? \`Bash · ${name}\` : "Bash"`（`:722`）。
- **section 0**：命令（`formatBashCommandLines` `:283-295`）：前缀 `$ ` + 可选 `cd <dir> && ` + 环境变量赋值，**dim 色**；命令正文**高亮**（`highlightCode(command, "bash")`）；多行命令首行带前缀、续行不带（`:294`）。折叠时用 `capPreviewLines`（`render/render-utils.ts:361-374`）保留**尾部**并在顶部加 `… N earlier lines (ctrl+o to Expand)`。
- **section 1**：`{ label: uiTheme.fg("toolTitle", "Output"), content: outputLines }`（`:629`）——**标签 `Output` 用 `toolTitle` 角色**。
- **输出行**：`formatOutputPaneLines({ lines, expanded, collapsedMaxLines: min(previewLines, previewWindow), edge: "tail", visual: true, width: contentWidth, styleLine: line => uiTheme.fg("toolOutput", replaceTabs(line)), uncapSixel: true })`（`:577-591`）——**`toolOutput` 色、尾部窗口、按视觉行截断**。
- **元信息行**：`bashStatsParts`（`:333-372`）产出 `Wall: 0.05s`、`Timeout: 30s`、`Timeout: disabled`、`Exit: N`、`Artifact: …`、`Backgrounded: …`、`Service: …` 等；用 `⟦…⟧` 包、`|` 分隔、dim 色（`:589-597`）。
- **截断警告**：`formatStyledTruncationWarning`（`:596-598`）加在输出行之后。
- **折叠预览行数**：`BASH_DEFAULT_PREVIEW_LINES = DEFAULT_TERMINAL_PREVIEW_LINES = 10`（`tools/bash.ts:33`、`render/render-utils.ts:119`）。

#### 用户 `!` / `!!` 命令（`BashExecutionComponent`）

- 与模型 bash **不是同一个组件**（blocks 篇 §7 已核实）。形状：**上下各一条横线夹住整块**（`buildExecutionFrame` `chat/execution-shared.ts:52-74`），首行 `$ <命令>`（`chat/bash-execution.ts:120`：`new Text(theme.fg(colorKey, theme.bold(\`$ ${command}\`)), 1, 0)`）。
- **边框色区分是否进上下文**：`const colorKey = excludeFromContext ? "dim" : "bashMode"`（`:102`）。
- **运行中**：`Loader` 一行 `Running… (<interruptKey> to cancel)`（`execution-shared.ts:64-70`）。
- **完成后**：`buildStatusFooter`（`:81-110`）产出 `… N more lines (ctrl+o to expand)` / `(cancelled)` / `(exit N)` / 截断提示，各自带角色（dim/warning/error）。
- 折叠预览 `PREVIEW_LINES = 20`（`:26`）；单行显示宽度上限 `MAX_DISPLAY_LINE_CHARS = 4000`（`:29`），超出时截断并加 `… [N visible columns omitted]`（`:31-40`）。

#### read

- **调用**（`tools/read.ts:477`）：`new Text(renderStatusLine({ icon: "pending", title: "Read", description: pathDisplay }, uiTheme), 0, 0)`——**`paddingX=0` 的裸 Text**（不是卡片）。`pathDisplay` 是 `formatReadPathLink(...)` + `:start-end` 行范围（`:470-476`）。
- **结果**（`:521-600`）：`framedToolCard`，head = `renderStatusLine({ icon: "success" | "error", title: "Read <path>:<range>", … })`；body 是 **code cell**（`renderCodeCell` `render/code-cell.ts:126-202`）或 **markdown cell**（读的是 `.md` 时，`:653-676`）。
- **code cell 的形状**：`renderOutputBlock({ header: title, headerMeta: meta, state, sections, width })`（`code-cell.ts:202`）；`codeMaxLines` 默认 **12**（折叠）、`outputMaxLines` 默认 **6**（`:136-137`）；`codeTail` 为真时保留**尾部**并加 `… N earlier lines`（`:186-190`），否则保留头部并加 `… N more lines`（`:191-194`）；**行号**用 dim 色、宽度 `max(2, digits(maxLineNo))`（`:160-180`）。
- **折叠时**：`read_body` 返回 hint `[read: N lines]`（`tools/read.ts` 的 `read_body`，见 Radian 侧 `tools.rs` 的对应物）。

#### edit / patch / write（diff）

- **调用**（`tools/edit.ts:1114-1150`）：`framedToolCard`，head = `renderEditHeader(width, uiTheme, { op, rawPath, rename, … })`（`:349-390`），**没有状态图标**（注释 `:1131-1136` 解释：head 是原生 scrollback 提交边界，动画图标会把它钉在顶部；liveness 靠尾部 `(preview)`/`(streaming)` 文字），`borderColor: "borderMuted"`、`contentPaddingLeft: 0`。
- **结果**（`:1464` 起）：`framedToolCard`，head 带 `icon: isError ? "error" : options.isPartial ? "partial" : "success"`（`:1559`）；body 是 diff 文本，用 `toolDiffAdded`/`toolDiffRemoved`/`toolDiffContext` 着色（`chrome/diff.ts:230-278`），**行号前缀**（`formatLine(prefix, lineNum, content)`）；一删一增相邻时有**行内高亮**（`:244-254` 的 `renderIntraLineDiff`）。
- **多个文件**：每个文件一个 section（`edit.ts:1180-1220` 的 `fileDiffSection`），折叠时 `PREVIEW_LIMITS.DIFF_COLLAPSED_HUNKS = 8`、`DIFF_COLLAPSED_LINES = 40`（`render/render-utils.ts:113-115`）。
- **write**（`tools/write.ts:626-660`）：head = `renderStatusLine(...)`，body 是内容预览（`framedToolCard` `:640`）。

#### grep / find / glob（plain，树形列表）

- **grep 是 `inline: true`**（`tools/grep.ts:274`）——即**不画框**（`tool-execution.ts:1049-1081` 的 `view.inline === true` → `frame: "inline"`、`preview: "none"`）。调用是一个 `new Text(renderStatusLine({ icon: "pending", title: "Grep", description: pattern, meta }), 1, 0)`（`tools/grep.ts:206-214`）。
- 结果用 `renderTreeList`（`render/tree-list.ts:42-...`）：`├─`/`└─` 连接符（dim 色）、折叠 `maxCollapsed: COLLAPSED_TEXT_LIMIT = PREVIEW_LIMITS.COLLAPSED_LINES*2 = 6`、行数上限同样 6（`tools/grep.ts:80`、`:325-335`），超出加 `… N more matches`（`renderTreeList` 的 `trailingSummary`）。
- **find**（`tools/find.ts:203-330`）：折叠 `COLLAPSED_HITS = 5`（`:108`），结果也是 `renderTreeList`；meta 里带 `N hits`、`N files read`、`τ 0.42`、`N tokens`、`$0.0000`、耗时（`:270-282`），**这是 omp 的「检索统计」写法**。
- **glob**（`tools/glob.ts`）：`renderFileList`/`renderTreeList`，`paddingX: 1`（`:183`、`:268`）。

#### web（web search / fetch）

- **web search**（`tools/web-search.ts`）：调用是 `new Text(renderStatusLine({ icon: "pending", title: "Web Search", description: query }), 0, 0)`（`:245`）；结果 `framedToolCard`（`:157`）带 `contentWidth`（`:157` 的解构），条目用 `renderTreeList`（`:23`）；有**provider badge**（`response.model`，`:329-330`）。
- **fetch / read URL**（`tools/fetch.ts:134-289`）：调用 `renderStatusLine({ icon: "pending", title: "Read", description, meta })`；结果 `framedToolCard`（`:233`、`:289`）；错误走 `icon: "error"`（`:231`）。

#### task / subagent

- **调用**（`tools/task.ts:646-660`）：`framedToolCard`，head = `renderStatusLine({ iconOverride: theme.styledSymbol("tool.task", "accent"), title: "Task", description: formatAgentHeaderLabel(args) })`——**用 `⇶` 图标而不是 pending 沙漏**（注释 `:641-645`：派发是非阻塞的，沙漏会误读成「turn 在等它」）。
- **body**：`context` section、`assignment` section（都是 Markdown，`ASSIGNMENT_FRAME_INSET = 3` 的缩进补偿，`:563-575`）、然后**每个 agent 一行**（`• <name>: <brief> [agentType]`，`:531-560`），折叠 `COLLAPSED_AGENT_LIMIT = 4`（`:541`），超出加 `… N more agents`。
- **结果**（`:1366-1470`）：同样的三段；agent 行按状态着色（`:1320-1340` 的排序/挑选逻辑）。

#### todo / memory / ask_user / 其它

- **todo**（`tools/todo.ts:365-393`）：`<checkbox> <content>`，状态色：completed `success` + 删除线、in_progress `accent`、abandoned `error` + 删除线、blocked `warning` + `(blocked: …)`、pending `dim`（被 subagent 点亮时 `accent`）。checkbox 字形 `theme.checkbox.checked/unchecked`（`☑`/`☐`）。**有一个「删除线逐字揭开」的动画**（`strikeRevealCount` `:355-362`）。
- **memory**（`tools/memory.ts:74`）：`format.bullet`（`•`）开头。
- **ask_user**（`tools/ask.ts:456-620`）：`framedToolCard`，head = `Ask` + muted `N questions`；每题一个带 label 的 section（label = dim `[id]` + `· multi` + `· options:N`）；题干用 `Markdown(text, 1, 0, mdTheme, { color: accent })`（`:448-449`）；选项行 `" " + dim(marker) + " " + label`，描述行 `"   " + dim("↳") + " " + description`（`:206-221`）；答完以后同一块里 marker 变 `success` 色（`:278-283`）；**取消**时一行 `<warning 图标> Cancelled`（`:271-273`）。
- **默认/未知工具**（`tools/default-renderer.ts:197-220`）：`plainToolCard`，`paddingX: 1, paddingY: 1, ignoreTight: true`——**背景按状态着色**（这是 blocks 篇 §1.5 提到的 `paddingY is 1 so background-tinted blocks get breathing room` 的出处）。
- **mcp**（`tools/mcp.ts:75/125/220`）：`paddingX: 0`。

### 3.5 ask 问答块

见上 `ask_user`，以及 `tui/src/overlays/ask-dialog.ts`（1734 行）——那是**面板**（§7.3）。

### 3.6 压缩摘要与系统提示

- **压缩摘要**（`tui/src/chat/compaction-summary-message.ts:44-88` 的 `DividerSummary`）：**一条规则线**，不是标记行。`render()` 产出 `["", divider(width), ""]`（`:64-66`），即**上下各一行空白**。
  - `#divider(width)`（`:68-87`）：`rule = theme.tree.horizontal`（`─`）；`hint = theme.sep.dot.trim() + " " + expandKeyHint()`（`"· ctrl+o"`）；`plainWidth = Bun.stringWidth(label + " " + hint)`；`remaining = width - plainWidth - 2`；`remaining < 4` 时**退化成裸 label**（`:76-79`）；否则左右横线平分（`left = floor(remaining/2)`、`right = remaining - left`），输出 `dim(rule×left) + " " + muted(label) + " " + dim(hint) + " " + dim(rule×right)`。
  - **label 按压缩方式变**（`:14-19`）：`remote → "remote-compacted"`、`soft → "soft-compacted"`、`handoff → "handed-off"`、`snapcompact → "snap-compacted"`、`shake → "shaken"`，未知 → `"compacted"`。
  - **金额 badge**：`compactionAmount`（`:21-26`）：`` `${formatNumber(before)}→${formatNumber(after)}` ``（`256K→20K`），`tokensAfter === undefined` 或 `before <= 0` 时为 undefined。
  - 展开后是 `Box(1, 1, bg=customMessageBg)` 里的 Markdown（`:180` 附近，`SummaryMessageComponent` 的 `#createDisclosure`）。
- **notice / 状态消息**（`tui/src/chrome/status-notice.ts`，64 行）：`new Text(message, 1, 0).setStyleFn(styleFn)` + 前置 `new Spacer(1)`（`:40-41`）——**无标记**。连续多条 notice **替换上一行**而不是追加（blocks 篇 §1.2 已核实）。**原生路径是 toast**（`:58-62`）：`node("toast", { text: plain, ttl: TOAST_TTL = 2400, role: "omp.toast.status" })`。
- **错误**：
  - transcript 里：`Error: <第一行>` + 后续行（`assistant-message.ts:1494-1510`），错误色，折叠上限 8 行（`MAX_TRANSCRIPT_ERROR_ROWS = 8`，`:47`）。
  - **钉在输入框上方的 banner**（`tui/src/overlays/error-banner.ts:26-51`）：`Spacer(1)` + `DynamicBorder(error)` + 首行 `✘ <第一行>`（**粗体 error 色**）+ 后续行 + 一行 dim 说明（`"Dismissed when you send your next message."`，`:47`）+ 底部 `DynamicBorder`。banner 上限 `MAX_BANNER_ROWS = 4`（`:12`）。**双横线夹住**，与 transcript 里的普通缩进行不同。
- **重试提示**（`pca/src/modes/interactive-mode.ts:7808-7832` 的 `syncRetryHintRow`）：`Spacer(1)` + `" <key> to retry"`，只在「最后一轮死在工具调用上」且状态行空闲时挂上；键是 `app.retry`（默认 `f5`）。
- **recap**（`tui/src/chat/recap-notice.ts`）：`※ recap: <text>`，dim + italic，`paddingX=1`；原生是 `row({ role: "omp.recap" })` + history 图标（`:29-35`）。
- **其它 notice 型块**：`chrome/message-notice.ts`（`Box(1, 1, bg=inverse(fg(severity)))`，`:70-71`）、`chrome/message-divider.ts`（`["", rule+label, ""]`，`:78-82`）、`chat/hook-message.ts`（`FramedMessageComponent`，折叠 `HOOK_COLLAPSED_LINES = 5`，`:8`）、`chat/custom-message.ts`、`chat/advisor-message.ts`（`▎` rail + severity 色，`:104-144`）。

---

## 4. 输入框（editor / composer）

### 4.1 组件与形状

- **editor**：`tui/src/components/editor.ts`（**5009 行**），`#borderStyle: EditorBorderStyle = "box"`（`:788`）。
- **composer 形状是一等公民**：`tui/src/components/composer/`（`types.ts` 113 行 + 8 个形状 + `registry.ts`）。`ComposerStyle` 接口（`composer/types.ts:96-113`）声明：`id`、`filledSurface?`、`sideBorders`、`verticalChrome`（0/1/2）、`statusAttachment`（`"top-border" | "top-band" | "top-rule-chip" | "none"`）、`bottomBar`（`"none" | "left" | "full"`）、`bottomBarGap`、`defaultPromptGutter`、`defaultPaddingX()`、`sideChromeWidth()`、`renderTop()`、`renderRow()`、`renderBottom()`。**注释（`types.ts:1-7`）明说这个契约的作用是「让 editor、`/settings` 预览、setup 向导预览三处用同一个 style 对象，永不漂移」。**
- **8 种内建形状**（`composer/registry.ts:12-21`，`BUILTIN_EDITOR_BORDER_STYLES` 在 `types.ts:17-26`）：

  | id | 外形 | 关键字段 | 出处 |
  |---|---|---|---|
  | `box` | 圆角框 + 状态嵌在**上边框** + 最后一行合并进下边框（`╰─ text … ─╯`） | `sideBorders: true`、`verticalChrome: 2`、`statusAttachment: "top-border"`、`bottomBar: "none"` | `composer/box.ts:1-85` |
  | `band` | **默认**（`cfgComposerShape` 的 default 是 `"band"`，`pca/src/modes/settings.ts:125-135`）：整条状态带贴着终端左缘，powerline 软开头，下方是**无框**的 `╰─ ` 提示 | `sideBorders: false`、`verticalChrome: 1`、`statusAttachment: "top-band"`、`defaultPromptGutter: "╰─ "` | `composer/band.ts:1-46` |
  | `claude` | 上下横线 + `❯ ` 提示；**右组**状态骑在上横线上（bg chip） | `verticalChrome: 2`、`statusAttachment: "top-rule-chip"`、`bottomBar: "left"` | `composer/claude.ts:1-39` |
  | `pi` | 上下横线 + 普通缩进文字，无提示符 | `verticalChrome: 2`、`statusAttachment: "none"`、`bottomBar: "full"` | `composer/pi.ts:1-37` |
  | `borderless` | 只有 `❯ `，无任何 chrome | `verticalChrome: 0`、`statusAttachment: "none"` | `composer/borderless.ts:1-37` |
  | `rule` | 一条上横线（带状态 chip）+ `❯ `，无下横线 | `verticalChrome: 1`、`statusAttachment: "top-rule-chip"` | `composer/rule.ts:1-49` |
  | `field` | 每行一条**填色**的字段，两端 accent 端点 `▐`/`▌` | `filledSurface: true`、`sideBorders: true`、`verticalChrome: 0` | `composer/field.ts:1-48` |
  | `rail` | **填色**，左侧一条 accent 竖条 `▎`，无框无规则线 | `filledSurface: true`、`sideBorders: true`、`verticalChrome: 0`、`bottomBar: "full"`、`bottomBarGap: true`、`defaultPaddingX: 1` | `composer/rail.ts:1-49` |
  | （+ `getComposerStyle` 对未知 id **回退到 `box`**） | | | `registry.ts:52-54` |

- **扩展可以注册形状**：`registerComposerStyle(style)`（`registry.ts:34-45`），拒绝内建 id 与重复 id。
- **ADR 0035 V3 的 `bar` = omp 的 `rail`**（逐条对上：`filledSurface` = 整行背景带、`ACCENT_RAIL = "▎"` = 左侧强调色竖条、`verticalChrome: 0` + `renderTop/renderBottom` 返回 undefined = 没有边框与上下规则线、多行时 `renderRow` 每行都带 rail 与填色 = 背景带随行数增高）。
- **`rail` 的状态栏在下方**（`bottomBar: "full"` + `bottomBarGap: true`，`composer/rail.ts:22-24`），**不是上方**——ADR 0035 决定 1 要上方，见主篇 §9.3 第 4 条。

### 4.2 背景带、竖条、多行、光标

- **填色**：`filledSurface: true` 的形状，行的前景由 `surfaceColor` 画（`ComposerStyle.filledSurface` 的注释 `types.ts:98-102`）；`getEditorTheme()` 给的 `surfaceColor` 是 `theme.bgFill("userMessageBg", theme.fgOnBg("userMessageText", "userMessageBg", text))`（`theme/tui-adapters.ts:373-374`）——**即输入框的填色与用户消息块同一个 `userMessageBg`**。这是一条重要的视觉一致性。
- **竖条**：`rail` 的 `ACCENT_RAIL = "▎"`（`composer/rail.ts:8`），色是 `accentColor`（= `theme.accentColor ?? borderColor`，`editor.ts:1396`；`getEditorTheme` 给的是 `theme.fg("accent", …)`，`tui-adapters.ts:368`）。
- **多行**：`renderRow` 每行都被 chrome 包一次（`editor.ts:1621-1634` 的循环里 `style.renderRow({...})`）。`rail`/`field` 的 `sideChromeWidth(paddingX) = paddingX + 1`（`rail.ts:31-33`）——1 格 rail + `paddingX` 格内边距。**`verticalChrome: 0` 意味着没有上下 chrome 行**，所以 `#getVisibleContentHeight()` 是 `maxHeight - verticalChrome`（`editor.ts:1198-1199`）。
- **光标**：
  - 非 Vim：`#getStyledInputCursor()`（`:1255-1265`）返回 `{ text: theme.symbols.inputCursor, width: visibleWidth(...) }`——`inputCursor` 在 ascii 档是 `|`、否则 `▏`（`tui-adapters.ts:170`）。注释解释了为什么不用 SGR blink（Ghostty/cmux 会留残影）。
  - 在字符上：`#cursorCell(grapheme)`（`:1250-1253`）→ **反显**（`\x1b[7m…\x1b[0m`），Vim insert 模式用**下划线**（`\x1b[4m`）。
  - Vim Normal/Visual：整个空格反显（`:1258-1260`）。
  - **行尾光标**：`#renderEndOfLineCursorAtWidthLimit`（`:1267-1280`），无侧边框的形状没有多余格子时**借最后一个字形的格子**（用下划线而不是反显，注释 `:1277-1279`）。
  - **硬件光标**：`emitCursorMarker = this.focused`（`:1404`），写一个 `CURSOR_MARKER`（`:1450`）让终端把硬件光标放在插入点（IME 候选框锚点）。
  - **IME 安全**：`imeSafeCursorTail`（`:1516-1527`）在 box 的最后一行留空右侧，防止 IME 预编辑把边框挤到下一行。

### 4.3 占位文字

- `placeholder?: () => string | undefined`（`editor.ts:773`），**每次渲染重新求值**（所以宿主可以从实时状态推导）。
- `#getPlaceholder()`（`:4961-4964`）：**有 autocomplete 打开、或编辑器非空、或有 prediction 时返回 undefined**。
- 位置：**右对齐**，且必须能留出 `PLACEHOLDER_MIN_GAP = 2` 格（`:457`、`:1417-1419`：`gap >= PLACEHOLDER_MIN_GAP ? { text: padding(gap) + placeholder, width: avail } : undefined`）。
- **`prediction`**（`:779-781`）：整条消息的「幽灵补全」，在缓冲区是它的严格前缀、光标在末尾、无 autocomplete 时画在光标后；**替代 placeholder**（`:4962`）；`Tab`/`Right` 插入剩余部分而不提交。
- omp 的 placeholder 内容是**手势提示**：`resolveComposerHint(ctx)`（`tui/src/prompt/composer-hints.ts:79-88`）按优先级取第一条适用的、且用户还没学会的（`learnedAfter: 3`）提示，渲染成 `accent(键) + dim(italic(说明))`。两条内建提示：`←←` 看运行中的 agent（`runningAgents > 0` 时）、thinking effort 键（对话未开始时）。**「学会就退休」这个机制 Radian 没有**。

### 4.4 粘贴大段文本

- 阈值：**`> 10 行` 或 `> 1000 字符`**（`editor.ts:755`、`:3391-3392` 的注释）。
- 默认行为：**折叠成一个 `[Paste #N]` 标记**（`#insertPaste` `:3196-...`），提交时展开（`getExpandedText` `:2861-2875`）。标记形如 `[Paste #2, +30 lines]` 或 `[Paste #2, 900 chars]`（正则 `:1023`：`/\[Paste #(\d+)(?:, (?:\+\d+ lines|\d+ chars))?\]/g`）。
- **宿主可以拦截**：`onLargePaste?: (text, lineCount, options) => boolean`（`:761`）——返回 `true` 则编辑器**什么都不插**、宿主自己处理（注释举例：给超大粘贴一个「包进代码块 / XML / 作为文件附件」的菜单），宿主再用 `insertPaste` 或 `insertText` 插回来。
- **标记是原子的**：`[Paste #N]` 与宿主注册的 chip 都是**不可分割的 token**（`:727-731` 的 `#atoms`），退格/删除整个删掉。
- **附件 chip**：`chip.image`/`chip.video`/`chip.paste`（`symbols.ts:247-249`）= `🖼`/`🎞`/`📄`。
- **bracketed paste 是迭代处理**：`handleInput`（`:1773-1781`）用一个 `while (next !== undefined)` 循环消化粘贴尾部的字节（注释：分片的粘贴流不会把调用栈撑爆）。

### 4.5 `@` 引用与 `/` 命令的补全菜单

- **provider 接口**：`AutocompleteProvider`（`tui/src/autocomplete.ts:236-298`）：`getSuggestions(lines, cursorLine, cursorCol, signal?, onPartial?)` 返回 `{ items, prefix } | null`；`applyCompletion(...)`；可选 `getInlineHint`、`trySyncSlashCompletion`、`trySyncInlineReplace`、`getForceFileSuggestions`、`shouldTriggerFileCompletion`。
- **内建实现**：`CombinedAutocompleteProvider`（`autocomplete.ts:612-...`），同时处理**斜杠命令**与 **`@` 文件引用**（`:721-737` 的 `@` 分支用 native `fuzzyFind`，`#dirCache` TTL 2 秒，`:619-623`）。
- **外观**：菜单就是 `SelectList`（`editor.ts:701` 的 `#autocompleteList`），挂在 editor 的 `render()` 末尾（`:1635-1647`）：
  ```ts
  const viewportRows = this.viewportRowsProvider?.() || process.stdout.rows || Number(Bun.env.LINES) || 24;
  autocompleteList.setMaxVisible(Math.max(3, Math.min(this.#autocompleteMaxVisible, viewportRows - result.length - 2)));
  result.push(...autocompleteList.render(width));
  ```
  即**行数上限 = `min(10, 视口行数 - 已渲染行数 - 2)`，下限 3**；`#autocompleteMaxVisible` 默认 **10**（`:712`，`prompt/composer.ts:52` 的 `autocompleteMaxVisible: 10`）。
- **`SelectList` 的两列布局**（`components/select-list.ts`）：主列宽默认 32（`DEFAULT_PRIMARY_COLUMN_WIDTH`，`:17`），描述列最小 10 格（`MIN_DESCRIPTION_WIDTH`，`:19`），**只有 `width > 40` 才分两列**（`:601`）；否则单列（`:620-626`）。光标 `❯ `（`:592` 的 `theme.symbols.cursor`）；溢出时 `ScrollView` 的 `scrollbar: "auto"` 占一列（`:474`）。
- **按键**（`editor.ts:1870-1960`）：
  - `Esc` 关菜单（若菜单可见则吃掉键；若菜单已隐藏则**让键落下去**，注释 `:1872-1876`）。
  - `Up`/`Down`/`PageUp`/`PageDown` 交给列表（`:1901-1925`）；**当前无候选时取消菜单并让键落下**（`:1906-1912`）。
  - `Tab` **总是接受**选中的候选（`:1927-1955`）；`Right` 在**行尾**时也接受（fish 风格，`:1883-1886`）。
  - `Enter`：在「已提交的斜杠命令上下文」里**应用并提交**（`:1957-1960`）；否则接受。
  - **过期检测**：接受前用 `#autocompletePrefixMatchesCursorText`（`:1933-1941`）核对缓冲区没被改过，否则**静默取消**。
- **斜杠命令的图标**：`getSlashCommandTypeIcon(name)`（`tui-adapters.ts:344-352`）在 **ascii 档返回 undefined**（注释：字母化图标列会读成噪音），所以 ascii 下这一列**整个消失**。

### 4.6 历史

- `#history: LocalHistoryEntry[]`（`editor.ts:740`），`#historyIndex = -1` 表示不在浏览（`:741`）。
- **上限 100 条**（`:1055`：`if (this.#history.length > 100) this.#history.pop();`）。
- `#navigateHistory(direction)`（`:1074-1094`）：`newIndex = #historyIndex - direction`（Up 是 `-1` → index 增加）；越界就 return（**不环绕**）。
- **浏览多行条目时锚点方向**：`cursorAnchor = direction === -1 ? "start" : "end"`（`:1092`）——Up 打开多行条目的**顶部**、Down 打开**底部**，这样一次按键就跨一条（注释指向 issue #99）。
- **草稿保留**：浏览时若条目带 `draft`（粘贴/原子标记的快照），恢复它（`:1081-1088`）；`#historyDraftActive` 记录状态。
- **持久化**：`HistoryStorage`（`:742`）+ `#setHistoryStorage`（`:991-994`）——宿主提供最近条目；**本地历史与持久历史分开**（注释 `:1018`：「Retain the current draft for local recall only, never persistent history」）。
- Radian 的对应物：`app.rs:1139-1143` 的 `history: Vec<String>` / `history_cursor: Option<usize>` / `history_stash: String`，浏览逻辑在 `app.rs:2971-2979`（`history_up`/`history_down`）、`:3442`/`:3457`。**没有条数上限**（`grep "history.pop\|history.truncate\|history.remove"` 零命中，`app.rs` 里只有 `history.push`）。**建议**：ADR 0035 不必管（100 条 vs 无限只是内存差异），但**若要照 pi 的键位（Up 在首行/末行才切历史）**，Radian 已经有（`README.md` 的键表：「Up / Down：on the first / last input line: the previous / next history entry」）。

---

## 5. 状态与反馈

### 5.1 spinner 的帧与位置

- **帧**：两套（`status` 粗 / `activity` 细），按 preset 取（`symbols.ts:1476-1489`），可由主题覆盖（`symbols.ts:1491-1511`）。
- **节拍**：`SPINNER_ADVANCE_MS = 80`（`components/loader.ts:16`）；重绘间隔 `RENDER_INTERVAL_MS = 1000/30`（`:14`，只在消息色是 `animated` 时用；否则用 80 ms 的 spinner 节拍，`:309`）。
- **背压**：`#scheduleTick`（`:341-380`）在每次 tick 后按上一帧的实际耗时算延迟，`backpressureDelayMs = min(200, max(完成耗时, 请求耗时)) * 9`（`:16`、`:370-379`）——注释：「Idle for nine times the full frame cost to keep animation at or below 10% CPU」。
- **`Loader` 的布局**（`:186-230`）：`render(width)` 先把 `super.render(width)` 的文本按 `paddingX=1` 拆成 `{leading, message, trailing, spinner, separator}`，然后**每帧只换 spinner 字形**（`#layoutFrames` 里每种宽度保留一个代表帧，`#syncText` 只在换帧时重设文本，`:392-397`）。首行前面加一个 `""`（`:224`）——即 spinner 行上方有一行空白。
- **位置**：
  - **ANSI**：`statusContainer` 在 dock 的「working loader / transient status」位置（`pca/src/modes/interactive-mode.ts:2181-2185`），它在 sticky todo/subagent HUD 之下、editor 之上。
  - **原生**：`describeWorkingRow(spec, cx, now)`（`components/loader.ts:44-90`）：`spinner(style: "starburst", tone: "accent")` + `elapsed(now - startedAt)` + `"·"` + shimmer 的 label + `grow: 1` 的填充 + `Stop/Cancel` 键 + 词（`:60-88`）。**结构是 `<spinner> <elapsed> · <label> …… <kbd> <Stop>`**。
  - **band composer 的品牌位**：`status-line/segments.ts:222-228`——turn 运行时 `pi` segment 变成 `<spinner 帧> <timer>`（`brandSpinnerFrame` `:237-240` 用 `SPINNER_ADVANCE_MS` 的时钟取帧，`brandTimer` `:243-248` 输出 `12s`/`5m`/`2h`）。

### 5.2 "Working…" / "Planning…" 这类状态行

- **默认词是 `"Working…"`**：`DEFAULT_WORKING_MESSAGE = "Working…"`（`pca/src/modes/interactive-mode.ts:481`）。
- **可以换**：`setWorkingMessage(message?)`（`:7702-7726`），扩展也可以调（`extensibility/extensions/types.ts:318`）。换词的来源：**流式工具意图**——`event-controller.ts:767` 的 `this.ctx.setWorkingMessage(trimmed)`，词来自模型流的 `intent` 字段（`normalizeIntent` `:156-166`）。
- **两个变体**（`WorkingRowVariant`，`components/loader.ts:22-32`）：`retry`（`{ attempt, max, delayMs }` → label `"Retrying · attempt 1 of 3"`，`pca/src/modes/controllers/event-controller.ts:2603`）与 `compaction`（label `"Compacting context…"` 或 `"Auto-compacting context…"`，`pca/src/modes/controllers/command-controller.ts:2015/2026`）。`retry` 用 `meter(style: "ring", size: "sm")` 画倒计时环；`compaction` 加一个不确定的 `progress`（`loader.ts:53-58`、`:82-84`）。
- **「Planning…」在 omp 里不存在**：`grep -rn "Planning…\|Planning\.\.\.\|\"Planning\""` 在 `pi-tui` 与 `pi-coding-agent` 的 `src` 下**零命中**。ADR 0035 背景段举的例子应删（见主篇 §9.4）。
- **词有 shimmer 动画**：`NATIVE_ACCENT_SHIMMER = { low: "dim", mid: "accent", high: "accent", bold: true }`（`pca/src/modes/interactive-mode.ts:483`），`renderWorkingMessage` 用它；`shimmerEnabled()` 关掉时 `animated` 不设，spinner 退回 80 ms 节拍（`:7670-7676`）。

### 5.3 turn 计时

- **原生 working row**：`elapsed(now - spec.startedAt)`（`components/loader.ts:63`），`startedAt = viewSession.runStartedAt ?? Date.now()`（`pca/src/modes/interactive-mode.ts:7681`）——**用会话的 run 起点，所以焦点切换后重建 loader 不会重置计时**。
- **品牌位**：`brandTimer(elapsedMs)`（`status-line/segments.ts:243-248`）：`<60s → "12s"`、`<3600s → "5m"`、否则 `min(99, h) + "h"`。
- **turn usage 行**：`turnElapsedMs(turnStartedAt, message)`（`overlays/usage-row.ts:36-45`）= `message.completedAt - turnStartedAt`，`> 0` 才有效；显示用 `formatDuration`（`:100`）。**没有 provider 报告的时长**，纯本地时间戳（注释 `:30-35`）。
- **status-line 的 `time_spent` / `time` segment**（`presets.ts:47-48`）。

### 5.4 通知与 toast

- **`StatusNotice`**（`chrome/status-notice.ts`）：ANSI 路径往 transcript 追加「一行空白 + dim 行」（`:40-41`）；**原生路径是 toast**（`:58-62`）：`node("toast", { text, ttl: 2400, role: "omp.toast.status" })`。`toast: false` 的 notice（例如 MCP 连接进度）**原生什么都不画**（`:28-32` 的注释）。
- **状态栏的 hook status**：`status` segment（`status-line/segments.ts:250-272`）把 `ctx.hookStatuses` 用 `theme.sep.dot` 连起来、accent 色。
- **`FooterComponent.setExtensionStatus(key, text)`**（`status-line/footer.ts:47-55`）：**按 key 覆盖**，不是追加；`render()` 里**按 key 字母序**拼成一行（`:406-413`），`sanitizeStatusText` 先去掉 ANSI 与控制字符。
- **原生 toast 的 TTL 是 2400 ms**（`chrome/status-notice.ts:9`）。

### 5.5 审批框

- **`ApprovalDialogComponent`**（`tui/src/overlays/approval-dialog.ts`，230 行）：
  - **形状**：`OverlayPanel(title, "omp.overlay.approval")`（`:76`）——**圆角框 + 标题嵌在上边框**；body 是 `PanelRows`，下面一条 `PanelDivider`，再一行 footer（`:77-81`）。
  - **标题**：`prompt` 的**第一行**（`:69-71`），详情是其余行（末尾空行剥掉）。
  - **详情**：默认**每行截断**（`truncateToWidth(theme.fg("accent", line), width, Ellipsis.Unicode)`，`:214-216`），`wrapDetails: true` 时折行（`:217`）。
  - **滚动**：详情超过 `MAX_DETAIL_ROWS = PREVIEW_LIMITS.COLLAPSED_ITEMS = 8`（`:32`）时用 `ScrollView`（`scrollbar: "auto"`，track muted / thumb accent，`:160-170`）。
  - **按键**（`:95-115`）：批准是 `app.approval.approve`（**默认 Ctrl+Enter**，注释 `:1-8` 解释为什么不用 Enter：避免用户在 prompt 出现的瞬间按下的 Enter 误批准），**并且 mount 后有 `APPROVE_ARM_MS = 500` 的静默期**（`:34`、`:101-104`）；`Esc` 拒绝（`matchesSelectCancel`）；`Up`/`Down`/`PageUp`/`PageDown` 滚动。
  - **超时**：`options.timeout` 时用 `CountdownTimer`，标题变成 `<title> (12s)`（`:85-92`）。
  - **footer**：`<approve keys> approve · <cancel> deny[ · ↑/↓ scroll]`（`:222-226`），dim 色。
  - **原生**：`describe()`（`:180-206`）画一个 `col({ role: "omp.editor.ask.approval" })`——**与 ask 面板同一个 role 前缀**，注释解释「Tern's console layout sizes an ask's rows to the composer column by that prefix」；内容是 title + details + `actionBar([countdown?, null, Deny, Approve])`。
- **Radian 的对应物**：`Modal::Approval`，高度 `approval_height`（`render.rs:889-896`），body `approval_body`（`:817-857`），按键行 `APPROVAL_KEYS = "[y] allow once   [a] always this session   [n] deny   [!] auto for this session"`（`:806-807`），最小高度 `MIN_APPROVAL_HEIGHT = 5`（`:885`），上限 `screen_height * 60 / 100`（`:876`）。**Radian 的按键行是自绘的一行**（注释说这是 验收要求的：长命令不能把按键挤出框），**omp 的按键在 footer 行**——两者都是「按键单独一行」，一致。

---

## 6. footer / 状态栏

### 6.1 各段

- **组件**：`tui/src/status-line/component.ts`（**3813 行**），`StatusLineComponent`（`:617`）。
- **preset**（`status-line/presets.ts:4-107`）：

  | preset | 左组 | 右组 | 分隔符 |
  |---|---|---|---|
  | `default` | `pi, vim, model, mode, collab, stream, path, git, pr, context_pct, cost` | `session_name` | `powerline-thin` |
  | `minimal` | `vim, path, git` | `session_name, mode, context_pct` | `slash` |
  | `compact` | `vim, model, mode, git, pr` | `session_name, cost, context_pct` | `powerline-thin` |
  | `full` | `pi, vim, hostname, model, mode, path, git, pr, subagents` | `session_name, cache_hit, token_in, token_out, token_rate, cache_read, cost, context_pct, time_spent, time` | `powerline` |
  | `nerd` | 同 `full` + `session` | 同 `full` + `cache_write`, `context_total` | `powerline` |
  | `ascii` | `vim, model, mode, path, git, pr` | `session_name, token_total, cost, context_pct` | `ascii` |
  | `custom` | `CUSTOM_STATUS_LINE_DEFAULTS.left` | `.right` | `powerline-thin` |

- **segment 目录（26 个）**：`status-line/schema.ts:2-29`：`pi, status, model, mode, path, git, pr, subagents, token_in, token_out, token_total, token_rate, cost, context_pct, context_total, time_spent, time, session, hostname, cache_read, cache_write, cache_hit, session_name, usage, collab, stream, vim`。
- **几个 segment 的具体形状**：
  - `pi`（品牌，`:206-235`）：空闲是 `theme.icon.omp`（`π`），运行中是 `<spinner 帧> <timer>`；focus-proxied 时是 `<ghost 图标> <agentId>`（warning 色）。
  - `model`（`:341-379`）：`withIcon(theme.icon.model, modelName)` + 可选 thinking 级别后缀（`theme.sep.dot + thinkingDisplay`）+ advisor badge + Anthropic 慢模式标签；色 `statusLineModel`。`compactThinkingLevel` 时图标换成 thinking 级别字形、后缀去掉（`:348-349`）。
  - `path`（`:567-598`）：worktree 里显示 `<projectName>[/<worktreeName>]`，否则 `shortenPath` + `clampPathLength(maxLength ?? 40)`；色 `statusLinePath`。
  - `git`（`:630-...`）：`⑂ <branch>` + `*<unstaged>`（`statusLineDirty`）+ `+<staged>`（`statusLineStaged`）+ `?<untracked>`（`statusLineUntracked`），各段可关（`segmentOptions.git.showStaged/showUnstaged/showUntracked`）。
  - `context_pct`（`:831-861`）：`withIcon(theme.icon.context, formatContextUsage(pct, window, tokens))` + 可选自动压缩脉冲图标（`speculation: "running"` 时闪烁、`"armed"` 时 accent）。
- **分隔符**（`status-line/separators.ts:4-34`）：`powerline`（`▶`/`◀` + endCaps）、`powerline-thin`（`>`/`<` + endCaps，**未知风格的兜底**）、`slash`（` / `）、`pipe`（` │ `）、`block`（`▌`）、`none`（空格）、`ascii`（`>`/`<`）。
- **`transparent`**（`types.ts:53-55`）：去掉 `statusLineBg` 填充与 powerline 端点，继承终端背景；代码里 `TRANSPARENT_BG_ANSI = "\x1b[49m"`（`component.ts:2936`），且 `capsVisible = separatorDef.endCaps !== undefined && !transparentBg`（`:3008`）——**透明时端点被丢掉**（注释：端点需要一个不透明填充来桥接，否则读成散落的字形）。

### 6.2 颜色

- `statusLineBg` 是背景；`statusLineSep` 是分隔符色；每个 segment 有自己的角色（`statusLineModel`/`statusLinePath`/`statusLineGitClean`/`statusLineGitDirty`/`statusLineContext`/`statusLineSpend`/`statusLineStaged`/`statusLineDirty`/`statusLineUntracked`/`statusLineOutput`/`statusLineCost`/`statusLineSubagents`），见 `dark.json:74-88`。
- **`sessionAccent`**（`types.ts:53`、`component.ts:3140-3170`）：按 session 名 hash 出一个强调色，用于 box 布局的**上下文 gauge 填充**（`#buildContextGaugeFill` `:3140-3200`）；`statusLine.sessionAccent: false` 关掉。
- **`contextLine`**（`schema.ts:42-43`，四档 `off`/`percentage`/`annotated`/`embedded`）：控制左右两组之间的**填充线**（`#buildContextGaugeFill` 的文档注释 `:3160-3175`）：`off` 纯 accent 实线；`percentage` 已用部分 accent、剩余用 border 色；`annotated` 加自动压缩的边界标记（`context.speculation` `╎` / `context.compaction` `┃`，`symbols.ts:418-419`）；`embedded`（**默认**）再把配置的 context segment 吸收成 gauge 上的百分比/窗口标签。

### 6.3 宽度不够时的省略顺序

`#renderStatusLine` 的溢出处理（`component.ts:3024-3125`）：

1. **先缩 `session_name`**：右组的 `session_name` 是唯一 elastic 的，最小保留 8 格，超出多少缩多少（`:3044-3068`，注释解释：整体丢掉它会让窄条（以及 ≤76 列的 composer 预览）完全没有标题）。
2. **再丢右组**：`while (totalWidth() > topFillWidth && right.length > 0) { right.pop(); … }`——**从末尾丢**（`:3066-3071`）。
3. **再缩 `path`**：`path` 是左组唯一 elastic 的，最小保留 8 格（`:3072-3108`）；它**重渲染**成一个更短的 `maxLength`，并且**迭代补偿**图标前缀的宽度（注释 `:3095-3103`：`maxLength` 管的是路径文字，不是图标前缀）。
4. **最后丢左组**：`leftOverflowDropIndex()`（`:3109-3121`）**优先保留 `path`**——从右往左找一个不是 `path` 的 segment 丢掉；如果只剩 `path` 就丢它。注释解释了为什么不用朴素的「从右往左」：那会把正常宽度的条压到只剩 model segment，**在 `model`/`mode`/`collab` 这些更不重要的东西之前就把 path 藏了**。
5. **原生路径的 drop priority**：`statusSegmentPriority(side, index, count)`（`:130-132`）= 左侧 `count - index`、右侧 `index + 1`——**外侧的 segment 优先保留**；`PINNED_NATIVE_SEGMENTS = { model, context_pct, context_total, git }` 有 `PINNED_NATIVE_PRIORITY = 1000`（`:76-82`）。

### 6.4 分组与间距

- `groupWidth(widths, capWidth, sepWidth) = sum(widths) + (n-1) * (sepWidth + 2) + 2 + capWidth`（`:3017-3022`）——**每个分隔符左右各 1 格，组首尾各 1 格**。
- `renderGroup(parts, direction)`（`:3127-3158`）：`bgAnsi + fgAnsi + " " + parts.join(" " + sepAnsi + sep + fgAnsi + " ") + " " + reset`，两端按方向加 cap。
- 左右组之间的 gap：`gapWidth = max(1, topFillWidth - leftWidth - rightWidth)`（`:3162`）；box 布局用 gauge 填充（`:3176`），plain 布局用空格（`:3168-3171`）。

### 6.5 另一种 footer

`status-line/footer.ts`（416 行）的 `FooterComponent` 是**另一套**：`pwd` + 统计行（token/context）+ 扩展状态行（`:401-413`）。**`grep -rn "new FooterComponent" tui/src` 零命中**，`grep -rn "FooterComponent" pca/src` 也零命中——**推断**：它是给**非交互/其它宿主**用的旧组件（例如 TUI 之外的渲染），18.10.4 的交互模式用 `StatusLineComponent`（`pca/src/modes/status-line-host.ts` + `modes/interactive-mode.ts:2163` 的 `setStatusComponent(this.statusLine)`）。**未 100% 核实它的调用方**。

---

## 7. 选择器与面板

### 7.1 面板的共享原语

- **`OverlayPanel`**（`tui/src/chrome/overlay-box.ts:218-350`）：
  - `constructor(title = "", role = "omp.overlay")`（`:229`）；标题经过 `collapseTitle`（空白折叠成单空格、trim，`:203-205`）。
  - `render(width)`（`:311-350`）：`innerWidth = max(1, width - 4)`；子组件各渲染一次（`PanelDivider` 贡献 0 行）；输出 `topBorder(width, title)` + 每个子组件的行（`row(line, width)` 包 `│ … │`）+ `PanelDivider` 处 `divider(width)` + `bottomBorder(width)`；`width < 4` 时整块 `truncateToWidth`（`:336-341`）。
  - **`topBorder(width, title, color?)`**（`:26-42`）：`╭` + `─` + ` <title> `（**加粗 + accent**）+ 填充 `─` + `╮`；标题 `truncateToWidth(\` ${title} \`, inner - 2)`。
  - **`divider(width)`**（`:44-47`）：`├` + `─`×(width-2) + `┤`。
  - **`row(content, width, color?)`**（`:55-58`）：`│` + ` ` + `padToWidth(content, width-4)` + ` ` + `│`——**左右各 1 格内边距**。
  - `PanelRows`（`:150-200`）：可固定高度的行区域（多了裁、少了补），`PanelDivider`（`:207-216`）：一个 `├──┤`。
  - **原生**：`describe()`（`:283-300`）→ `overlayCard(role, title, children)`，框由终端画。
- **两列布局**（`:60-110`）：`splitDividerCol(sidebarWidth) = sidebarWidth + 3`、`splitBodyWidth(width, sidebarWidth) = max(0, width - sidebarWidth - 7)`、`topBorderSplit`（标题 + `┬`）、`dividerSplit`（`┴`）、`splitRow`（`│ sidebar │ body │`，各 1 格内边距）。

### 7.2 列表：`SelectList`

`tui/src/components/select-list.ts`（约 700 行）：

- **项**：`SelectItem { value, label, description?, icon?, iconName?, nativeDetail?, state?, hint?, disabled?, confirmation?, searchText? }`（`:56-80`）。
- **两列布局**（§4.5 已述）：主列 32 / 描述最小 10 / `width > 40` 才分两列。
- **光标行**：`❯ ` + 加粗（`selectedPrefix` + `selectedText`，`tui-adapters.ts:326-328`）。
- **搜索**：`#search: Input`（`:112`）；策略 `search: "overflow" | "always" | "never"`（默认 `overflow`，即**只有项数 > maxVisible 才可搜**，`:145-148`、`#canEditSearch` `:687-690`）；状态行显示 `  Search: <query>` 或 `  Type to search`（`#renderStatusLine` `:659-674`）。
- **确认**：`item.confirmation` 存在时，激活要**第二次 Enter**，且该文本变成状态行（`:74-75`、`#statusText` `:676-686`）。
- **滚动**：`getMenuWindow(rowCounts, selectedIndex, visualBudget)`（`:432`）+ `ScrollView`（`scrollbar: "auto"`，track `scrollInfo`、thumb `selectedPrefix`，`:474-479`）。
- **鼠标**：`routeSelectListMouse`（`:186-188`）、`setHoverIndex`（`:174-176`）——hover 用 `theme.hovered`（`selectedBg` 背景，`tui-adapters.ts:334`）。
- **原生**：`describe()`（`:196-230`）→ `list` 节点，带 `selected`/`filter`/`empty`/`max` props；`#describeItem`（`:242-283`）→ `item` 节点，带 `icon`/`label`/`detail`/`value`/`disabled`/`tone`。

### 7.3 三个具体面板

- **模型选择**（`overlays/model-picker.ts`，477 行）：`bottom-anchored floating overlay`，内容是一个 `ModelBrowser`（`overlays/model-browser.ts`，2280 行），**没有 provider 侧栏**（`:1-6` 的文档注释）；搜索以 `@` 开头时暴露 `ctrl+p` 的 quick roles。**更大的 `/model` 界面**是 `overlays/model-hub.ts`（4209 行）。
- **会话选择**（`overlays/session-selector.ts`，1945 行）：`SessionSelectorComponent extends OverlayPanel`（`:1376`），标题 `options.title ?? "Resume Session"`（`:1452`），**scope 标签**加在标题后缀（`:1513-1515`，例如 `(current folder)`）；每项 3 行（无 title）或 4 行（有 title，`:1260`）；有**预览面板**（`describeSessionPreview` `:150-180`）与 fact grid。
- **设置**（`overlays/settings-selector.ts`，1999 行）：**全屏框**（不是 `OverlayPanel`），结构是「顶框 + tab 行 + 分隔线 + [搜索行] + 分隔线 + 列表 + footer hint + 底框」（`:734-783`）；`TabBar`（`components/tab-bar.ts`）画 `Settings:  Appearance   Model   …  (tab to cycle)`（`:1-6` 的文档注释），**窄时用 `short` 形式（只有图标）**（`Tab.short` 字段，`tab-bar.ts:24-26`）；footer hint 按上下文变（`:91-144`，`hintsRow`）。
- **审批**：见 §5.5。
- **ask**（`overlays/ask-dialog.ts`，1734 行）：见主篇 §9.3 第 7 条与 `docs/adr/0024` 修订 1 的对照。

### 7.4 按键提示的渲染

- `keyHint(action, description)`（`chrome/keybinding-hints.ts:52-56`）：`dim(键) + muted(" " + 说明)`。
- `editorKey(action)` / `editorKeys(...)`（`:19-26`）：取**第一个**绑定；`formatKeyHint` 把键名转成符号（`enter → ⏎`、`escape → ⎋`、`tab → ⇥`，`symbols.ts:659-665`）。
- `formatTooltipKey(key)`：工具提示里用的键文本。
- `actionHint` / `hintsRow`（`native/overlay.ts`）：原生按钮与提示行。

---

## 8. 窄终端与 resize

### 8.1 逐组件的退化规则

| 组件 | 退化规则 | 出处 |
|---|---|---|
| **状态栏** | 缩 `session_name`（到 8）→ 丢右组（从末尾）→ 缩 `path`（到 8）→ 丢左组（保 `path`） | `status-line/component.ts:3044-3121` |
| **Markdown 表格** | `overhead + columns*3 > available` → **回退成原始 `\|` 文本**（折行） | `components/markdown.ts:4026-4034` |
| **Markdown 表格列** | 先降到每列 1，按最长词比例分配；再按比例收缩 | `:4060-4125` |
| **Markdown 分隔线** | `min(width, 80)` | `:3528` |
| **Markdown 树形字符画** | `width < MIN_TREE_CONTENT_WIDTH = 8` 时**放弃悬挂缩进**，走普通折行 | `:414`、`:480`、`:487` |
| **Markdown 引用** | 内容宽 `max(1, width - 2)` | `:3467` |
| **Markdown 列表** | 前缀宽 `>= width` 时截断前缀并整行折 | `:3800-3810` |
| **工具卡片（framed）** | `lineWidth < max(cap+2, paddingL+paddingR+2)`（`cap` 是 3 格 `─`）时**整块裁到 lineWidth 加 `…`** | `render/output-block.ts:196-206` |
| **工具卡片（内容）** | 每行 `wrapTextWithAnsi(line, contentWidth)`；内容宽 `max(0, lineWidth - 2 - padL - padR)` | `:107-110`、`:155-160` |
| **代码块（cell）** | 行号宽度 `max(2, digits)`；超宽行不折，靠 `truncateToWidth` | `render/code-cell.ts:160-180` |
| **bash/eval 输出** | 单行可见列上限 `MAX_DISPLAY_LINE_CHARS = 4000`，超出加 `… [N visible columns omitted]` | `chat/execution-shared.ts:29-40` |
| **bash/eval 折叠** | `min(previewLines, previewWindowRows())`，`previewWindowRows() = max(6, rows - 20)` | `render/render-utils.ts:337-347` |
| **压缩摘要** | `remaining < 4` → 退化成**裸 label**（不画横线） | `chat/compaction-summary-message.ts:76-79` |
| **消息分隔线** | `ruleWidth = min(ruleWidth ?? 10, width - labelWidth - 1)`；`< 1` 时只留 label | `chrome/message-divider.ts:78-82` |
| **错误 banner** | `formatErrorBlock(message, contentWidth, MAX_BANNER_ROWS = 4, …)` | `overlays/error-banner.ts:12`、`:42` |
| **审批框** | 详情行截断（默认）或折行（`wrapDetails`）；高度上限 = 屏高 60%，最小 5 | `overlays/approval-dialog.ts:32`、`:214-217`；Radian 侧 `render.rs:876-885` |
| **ask 面板** | 高度 `max(12, floor(rows*0.7))`，**固定不随内容变**（只有视口变化才重测）；题干最多 `MAX_HEADER_ROWS = 4` 行；选项描述最多 `MAX_DESC_ROWS = 2` 行；标签名截到 `MAX_HEADER_CHIP_WIDTH = 16` | `overlays/ask-dialog.ts:116-127`、`:1121-1125`、`:218-225` |
| **选择器列表** | 主列宽 `clamp(widest, min, max)`（默认 32）；`width > 40` 才分两列，否则单列；`maxVisible` 由宿主按视口 clamp | `components/select-list.ts:17-19`、`:601`、`:649-658` |
| **autocomplete 菜单** | `max(3, min(10, viewportRows - 已渲染行数 - 2))` | `components/editor.ts:1640-1643` |
| **editor（无侧边框）** | `displayWidth > lineContentWidth` 时 `sliceByColumn` 裁；行尾光标借最后一格 | `components/editor.ts:1452-1455`、`:1575-1585` |
| **editor（`width` 很小时）** | `lineContentWidth === 0` 时只画提示符的首字形 + 光标 | `:1457-1490` |
| **box composer 的顶框** | 状态内容放不下时 `truncateToWidth(content, topFillWidth - 1)` | `components/composer/box.ts:40-49` |
| **rule composer 的 chip** | `chipWidth > width - 2` 时截断（保留左右各 1 格横线） | `components/composer/rule.ts:12-24` |
| **band composer 的顶行** | `topBorder.width > width` 时截断 | `components/composer/band.ts:33-36` |
| **`OverlayPanel`** | `width < 4` 时整块 `truncateToWidth` | `chrome/overlay-box.ts:336-341` |
| **二维码** | `width < minWidth` 时返回提示文本 | `chrome/collab-qrcode.ts:54` |
| **transcript 视口** | 行数不够时按「普通块优先于 tool-activity」分配，工具卡片收成 compact 形式（`setToolActivityVisible(false)` → `render` 返回 `[]`） | `chrome/transcript-container.ts:606-620`、`chrome/tool-activity.ts:38-40` |

### 8.2 resize

- **宽度变化会让所有缓存失效**：`Markdown` 的签名含 `width`（`components/markdown.ts:2692-2716` 的 `#renderSignature` / `#renderCacheKey`），`SelectList` 按宽度缓存，`Loader` 按宽度保留代表帧（`components/loader.ts:135-143`）。
- **状态栏按可用宽度重算**：`#buildStatusLine(width, layout, previewTitle)`（`:2828`）按 `width` 缓存（`:2840-2848`）。
- **编辑器的 `#getContentWidth(width, paddingX)`**（`editor.ts:896-897`）：`max(0, terminalWidth - borderWidth*2)`。
- **`tui.resizeScrollback`**（`pca/src/modes/settings.ts:362`）：resize 时是否清终端 scrollback（一个 omp 特有的开关）。
- **`previewWindowRows()` 每次渲染重算**（`render/render-utils.ts:344-347`），所以视口变化会立刻改变折叠预览的行数。

### 8.3 「窄」的判据汇总

omp 里出现的所有宽度阈值（便于 Radian 对齐）：

| 阈值 | 含义 | 出处 |
|---|---|---|
| `width > 40` | `SelectList` 是否分两列 | `components/select-list.ts:601` |
| `remainingWidth > 10` | `SelectList` 描述列是否显示 | `:610`（`MIN_DESCRIPTION_WIDTH`） |
| `width >= 8` | Markdown 树形字符画的悬挂缩进 | `components/markdown.ts:414` |
| `width - labelWidth - 1 >= 1` | 消息分隔线是否画横线 | `chrome/message-divider.ts:79-82` |
| `width - plainWidth - 2 >= 4` | 压缩摘要是否画横线 | `chat/compaction-summary-message.ts:75-79` |
| `width >= 4` | `OverlayPanel` 是否画框 | `chrome/overlay-box.ts:336` |
| `width > 4` | `row()` 是否给内容留 4 列 | `:57` |
| `lineWidth <= 0` | 输出框是否只画两端字符 | `render/output-block.ts:167` |
| `lineWidth < max(5, padL+padR+2)` | 输出框是否裁框 | `:196` |
| `2 * plainWidth <= width` | 1 级标题是否放大 2 倍 | `components/markdown.ts:3369` |
| `availableForCells < numCols` | 表格是否回退成原始文本 | `:4028` |
| `viewportRows - 已渲染 - 2 >= 3` | autocomplete 菜单的最小行数 | `components/editor.ts:1642` |
| `rows * 0.7 >= 12` | ask 面板的高度 | `overlays/ask-dialog.ts:1121-1125` |

### 8.4 与 Radian 现状的差异（窄终端）

- **Radian 的状态栏**：`fit_segments`（`render.rs:1115-1147`）**只丢不缩**（除了加 `…`），且**最后一段永不丢**（`:1130-1134`）。omp 会**先缩 elastic 的 segment**（`session_name`、`path`）再丢。**建议补「先缩 `cwd`」**（主篇 §9.3 第 8 条）。
- **Radian 的 `@` 补全列表**：`completion_rows`（`render.rs:1756-1775`）取 `COMPLETION_MAX_LINES = 10`（`:1750`），超出加 `+N more`（`:1768-1772`），每行 `truncate_columns`（`:1778-1787`，按显示宽度裁、不劈宽字符）。**没有「按视口 clamp」**（omp 有，`editor.ts:1640-1643`）。**建议补**（否则小终端里菜单会把输入框挤出屏幕）。
- **Radian 的 `md_table`**：`overhead + columns * min_column(3) > available` 时回退（`blocks.rs:1744-1745`）——**与 omp 的判据一致**（omp 的 `availableForCells < numCols` 更宽松，但两边的 min 都是 3）。**建议不改**。
- **Radian 的 Markdown 分隔线**：`width.clamp(1, 40)`（`blocks.rs:1577`）——**没有 80 上限的问题**（40 更小）。
- **Radian 的审批框**：`screen_height * 60 / 100` 上限（`render.rs:876`）——**与 omp 的 0.7 接近**（omp 用于 ask 面板）。
- **Radian 的问题表单**：`MAX_QUESTION_ROWS = 18`（`render.rs:1589`）——**固定上限**，不是屏高比例。**建议改成屏高比例**（omp 的 `max(12, 0.7*rows)`），理由：18 行在 20 行的终端里会顶掉输入框。

### 8.5 与 Radian 的逐块对照表

| omp 的块 | omp 形状（出处） | Radian 的对应物 | 差异与建议 |
|---|---|---|---|
| 用户消息 | `Markdown(text,1,1)` + `userMessageBg` 背景 + OSC 133 首尾标记（`chat/user-message.ts:174`、`:350-352`） | `EntryKind::User`，`BodyLine::pad(0)` + 正文 + `pad(end)`，`card_band` 给 `theme.user` 背景（`blocks.rs:817-823`、`render.rs:274`） | **一致**（都是「背景带 + 上下带背景的 pad 行」）。**Radian 无 OSC 133**——**建议不做**（Radian 不面向多路复用器的 prompt 识别，且它是终端副作用）。 |
| 助手正文 | `Markdown(text,1,0)`，无标记无背景（`assistant-message.ts:1339`） | `EntryKind::Assistant`，前缀 `"  "`（2 格），无背景（`render.rs:194`、`blocks.rs:824`） | **差异**：omp 用 `paddingX=1`（第 2 列起），Radian 用 2 格前缀（第 3 列起）。**ADR 0028 第 2 条定 2 格**——**建议保留 Radian 的**（更整齐，因为所有块都从第 3 列起）。 |
| thinking | `Markdown(text,1,0)` + `thinkingText` + italic；折叠头 `Thought for 12s` / 流式 `spinner + Thinking… + elapsed + tok/s`（`assistant-message.ts:1340-1343`、`:931-943`） | `EntryKind::Reasoning`，前缀 `"~ "`，折叠 `▸ thinking (N lines)` + live 时 4 帧 braille（`render.rs:195`、`blocks.rs:1045-1055`） | **差异**：omp 显示**耗时**，Radian 显示**行数**；omp 的 live 头是**结构化的一行**，Radian 是一个合成字符串。**ADR 0035 说「折叠规则不变」→ 保留 Radian 的**。**但建议补**：live 时也显示耗时（omp 的 `elapsed`），因为「正在思考多久」比「有多少行」有用。 |
| 工具卡片 | `ToolCard`（framed/plain × 7 phase），`renderStatusLine` 一行 head，`Output` section，`⟦…⟧` 元信息行（`render/tool-card.ts`、`render/output-block.ts`） | **两个条目**：`ToolCall`（前缀 `"-> "`）+ `ToolResult`（前缀 `"<- "`），body 由 `tools.rs` 的 `body_plan` 决定（`render.rs:196-197`、`tools.rs:141-183`） | **ADR 0028 已决定合成一张卡片**，未实现。见主篇 §9.3 第 1 条。 |
| bash（模型） | head（状态行）+ section 0（`$ cmd`，dim 前缀）+ section 1（`Output`）+ 元信息行（`tools/bash.ts:501-640`） | `shell_head`（`shell: <cmd> (exit N)` 一行）+ body（stderr 段用 `Warn` 角色）（`tools.rs:337-388`） | **ADR 0028 的三段式**要照 omp；Radian 现在把命令与退出码挤在一行。**建议照 omp**。 |
| bash（用户 `!`） | 上下横线夹块 + `$ cmd` 首行 + 边框色区分上下文（`chat/bash-execution.ts:102/120`、`execution-shared.ts:52-74`） | `EntryKind::Bash { exclude_from_context }`，前缀 `"$ "`，marker 色区分（`render.rs:208`、`:229-237`） | **ADR 0033 已定**（blocks 篇 §5.1/§6.2）。Radian 现在**没有上下横线**——ADR 0033 的建议里有（「照 omp 的两条横线夹块」）。**建议照 ADR 0033 做**。 |
| read | 调用是 `Text(..., 0, 0)`（无框）；结果是 code cell（12 行折叠 + 行号）（`tools/read.ts:477`、`render/code-cell.ts:136-137`） | `read_head`（`read: <path> (lines a..b)`）+ `read_body`（折叠成 `[read: N lines]`）（`tools.rs:390-424`） | **一致**（都是「折叠时只给行数」）。**差异**：omp 展开后有**行号**与语法高亮（code cell），Radian 展开后是纯文本。**建议照 omp 加行号**（ADR 0028 R4 的表格里 `read` 一行写「展开后带行号」——**ADR 已经要求了**）。 |
| edit / patch / write | framed 卡片，head 无状态图标，diff 着色 + 行号 + 行内高亮（`tools/edit.ts:1114-1150`、`chrome/diff.ts:230-278`） | `patch_head`（`patch: <path> +N/-M`）+ `diff_body`（`diff_role` 三色）（`tools.rs:426-455`、`:312-334`） | **一致**（三色 + 头部计数）。**差异**：omp 有**行号**与**行内高亮**。**建议行号照做**（ADR 0028 R4 未提行号——**该补**）；**行内高亮不做**。 |
| grep / find / glob | `inline: true`（无框）+ `renderTreeList` 的 `├─`/`└─` + `… N more`（`tools/grep.ts:274`、`render/tree-list.ts:42`） | 通用渲染器（`generic_head` + `text_body_lines`）（`tools.rs:273-280`） | **差异**：Radian 没有 `grep`/`find` 的专用渲染器（`renderer_for` 的表里没有它们，`tools.rs:213-270`）。**建议加**（ADR 0028 R4 的表格也没列 grep/find——**该补**），至少给一个「结果数 + 文件树」的 head。 |
| web / fetch | `framedToolCard` + provider badge（`tools/web-search.ts:157/329`、`tools/fetch.ts:233/289`） | 无（Radian 首版没有 web 工具） | 不适用。 |
| task / subagent | framed 卡片 + agent 行（`• name: brief`）+ 折叠 4 个 + `… N more agents`（`tools/task.ts:646-660`、`:541`） | 无（ADR 0030 的编排原语还没落地） | 待 ADR 0030。 |
| ask_user | framed 卡片 + `Ask` + `N questions` + 每题一个带 label 的 section + `○`/`☐` marker + `↳` 描述（`tools/ask.ts:456-620`） | `ask_head`（`ask: N questions`）+ `plain_body`（`tools.rs:538-...`） | **ADR 0024 R1 已定**（带框 + 分隔线 + `○`/`☐`），未实现。**建议照 ADR 0024 做**。 |
| notice | `Text(message, 1, 0)` + `Spacer(1)`，无标记；原生是 toast（`chrome/status-notice.ts:40-41`、`:58-62`） | `EntryKind::Notice`，前缀 `"* "`（`render.rs:198`） | **差异**：Radian 有 `* ` 标记，omp 没有。**建议保留 Radian 的**（ADR 0028 说保留标记列）。 |
| 错误（transcript） | `Error: <第一行>` + 后续行，错误色，折叠 8 行（`assistant-message.ts:1494-1510`、`:47`） | `EntryKind::Warning`（前缀 `"! "`）或 `Notice`；**没有专门的错误块** | **建议补**：ADR 0028 R5 说「审批记录、待决请求的结果、记忆注入：显示为 `Notice` 样式的一行」，但**没有说模型/provider 错误**。omp 有专门的错误块（含折叠上限 8 行）。**建议 ADR 0028 补一条「provider 错误：`Error: …` 开头，折叠上限 8 行」**。 |
| 错误（banner） | `Spacer(1)` + `DynamicBorder(error)` + `✘ <第一行>` + dim 说明 + `DynamicBorder`，上限 4 行（`overlays/error-banner.ts:26-51`、`:12`） | **没有** | **建议补**（omp 的 banner 是「钉在输入框上方」的，Radian 现在错误会滚走）。 |
| 压缩摘要 | `["", "──── 📷 compacted · ctrl+o ────", ""]`（`chat/compaction-summary-message.ts:64-66`、`:68-87`） | `EntryKind::Compaction`，前缀 `""`，规则线 `── context compacted (rev N, T→T) ──`（`render.rs:200`、`app.rs:80-93`） | **一致**（都是规则线，上下各一行）。**差异**：omp 有 `📷` 图标与 `ctrl+o` 提示，label 按压缩方式变（`remote-compacted` 等）。**Radian 的折叠开关是同一个**（`blocks.rs:893-901` 用 `expanded(entry, modes.tools.fold == ToolFold::Expanded)`，而 `Ctrl-O` 切的就是 `tool_display.fold`，`app.rs:3049-3057`）——**建议在压缩摘要的规则线上补 `Ctrl-O` 提示**（现在没有，用户不知道能展开）。 |
| recap | `※ recap: <text>`，dim italic（`chat/recap-notice.ts`） | 无 | 不适用（Radian 没有 idle recap）。 |
| hook / custom 消息 | `FramedMessageComponent`，`customMessageBg` 背景，折叠 5 行（`chrome/message-frame.ts`、`chat/hook-message.ts:8`） | 无（ADR 0027 C3 的贡献者是数据不是消息） | 待扩展协议 ADR。 |
| advisor 消息 | `▎` rail（severity 色）+ 逐条 note（`chat/advisor-message.ts:104-144`） | 无 | 不适用。 |
| todo | `<checkbox> <content>` + 状态色 + 删除线揭开动画（`tools/todo.ts:365-393`） | 无（ADR 0030 的 todo 还没落地） | 待 ADR 0030。 |

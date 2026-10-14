# omp（oh-my-pi）的「块」设计与 `!` 运行命令 —— 调研（对照 Radian transcript）

日期：2026-10-14 · 调研者：agent · 状态：只读调研，未改动任何代码

手测后提出两件事：Radian transcript 里各种内容块的 padding 参差不齐（`~ ▸` / 两格缩进 / `-> ` / `<- ` / `* ` 的标记与缩进各不相同；块之间空行有的有有的没有；工具调用与结果被拆开），要求「查一下 omp tui 的块是怎么设计的」；另外要 `!` 运行命令。本文回答这两问，并给出对 Radian 的建议。

## 0. 版本与出处（**重要**）

本次调研开始时本机 `omp` 是 **17.3.8**，中途 owner 升级到 **18.10.4**（`<home>/.bun/bin/omp`，`omp/18.10.4`，2026-10-14 15:57 替换）。两个版本的 TUI 结构差别很大，本文**以 18.10.4 为准**。

- **18.10.4 的源码**不在 `node_modules` 里（`<home>/.bun/install/global/node_modules/@oh-my-pi/*` 仍是 Aug 19 的 17.3.8，`package.json` 写 `17.3.8`）。本文 18.10.4 的行号来自 npm registry 的 tarball（`https://registry.npmjs.org/@oh-my-pi/pi-coding-agent/-/pi-coding-agent-18.10.4.tgz`，shasum `48c629464ce055e8dbca0eaef8c0f8e156e34d83`，与 registry 公布值一致；`pi-tui` 同版本 shasum `89db1e65de97f0c877eac4cd72ac5757256b806e`，一致）。两个包都带 `src/`。解包在 `.scratch/omp184/`（临时目录，**写完本文即删除**，不在交付物里）。
- 出处写法：`文件:行号`，文件相对各自包的根。`pca/` = `@oh-my-pi/pi-coding-agent`，`tui/` = `@oh-my-pi/pi-tui`。
- **许可与仓库**：`pca/package.json` 写 `"license": "MIT"`、`"repository": "git+https://github.com/can1357/oh-my-pi.git"`、`"author": "Can Boluk"`、`contributors` 含 `Mario Zechner`、`homepage: https://omp.sh`。第三方资料说的「MIT、can1357/oh-my-pi」**已核实**（以包内 `package.json` 为准；本机包未附 `LICENSE` 文件，`pi-tui`/`pi-coding-agent` 的 tarball 里都有 `LICENSE`，本次未逐一读取正文）。
- **没做的事**：没有运行 `omp`；没有读 `~/.omp`、`~/.pi` 或任何用户数据、凭据目录；只读安装包与 npm tarball 里的代码和文档。
- **pi 对照**：`<home>/.bun/install/global/node_modules/@earendil-works/pi-coding-agent`，`1.2.0`，**只有编译后的 `dist/`**（无 `src/`），所以 pi 的结论只能来自 `dist/` 与随包的 `docs/`（出处会标注是文档还是代码）。

### 0.1 18.10.4 的结构变化（影响怎么找代码）

17.3.8 里聊天组件在 `pca/src/modes/components/*`；18.10.4 把它们**移进了 `pi-tui`**：

- `pca/src/modes/components/index.ts` 现在基本只是一个 re-export barrel，全部指向 `@oh-my-pi/pi-tui/...`（`pca/src/modes/components/index.ts:1-40`）。
- 聊天块在 `tui/src/chat/`（`assistant-message.ts`、`tool-execution.ts`、`user-message.ts`、`bash-execution.ts`、`chat-transcript-builder.ts`…）。
- 外壳在 `tui/src/chrome/`（`transcript-container.ts`、`chat-block.ts`、`status-notice.ts`…）。
- 渲染原语在 `tui/src/render/`（`output-block.ts`、`status-line.ts`、`tool-card.ts`…）。

- 18.10.4 还把主题与符号表也搬进了 `pi-tui`：主题在 `tui/src/theme/`（`dark.json`、`light.json`、`symbols.ts`、`defaults/`…），17.3.8 时在 `pca/src/modes/theme/`。两版的 `dark.json` 行号相同。

`pca/src/modes/components/index.ts` 里对应的行（18.10.4）：`assistant-message`、`bash-execution`、`tool-execution`、`user-message`、`compaction-summary-message`、`read-tool-group` 等全部从 `@oh-my-pi/pi-tui/chat/*` 导出。

---

## 1. 块的种类与外观

### 1.1 总原则：容器管块间距，块只管自己的内容

这是理解 omp 排版的**第一把钥匙**，两版都成立：

> 每个块渲染出「只有可见正文」的行；**块与块之间的空行由容器拥有**。

18.10.4：`tui/src/chrome/transcript-container.ts`

- `isPlainBlank(line)`：空行或纯空白、**且不含 ANSI**（`transcript-container.ts:164-166`）。注释说明：纯空白的 padding 行会被当作「间隔」剥掉，而**带背景色的** padding 行（转义序列里有非空白）会被保留，作为块视觉设计的一部分。
- 这条判据的实际效果由 `Markdown` 决定：`Markdown` 的 `paddingY` 行在**有** `bgColor` 时会被背景包住（`tui/src/components/markdown.ts:2255-2264` 的 `#renderEmptyPaddingLines`：`const line = bgFn ? applyBackgroundToLine(emptyLine, width, bgFn) : emptyLine;`），因此**用户消息（`Markdown(text, 1, 1, …, { bgColor })`）的上下留白是带背景的、能存活**；而没有 `bgColor` 的块（助手正文 `paddingY=0`、thinking `paddingY=0`）根本不产生留白行。**这就是 omp 里「块的上下留白」真正的实现方式**——不是靠块间空行，而是靠「带背景的 paddingY 行」。`Text` 组件同理（`tui/src/components/text.ts:160-168`，`#customBgFn` 存在时 paddingY 行也带背景）。
- `trimBlankEdges(rows)`：剥掉块渲染结果首尾的纯空白行（`transcript-container.ts:203-210`）。
- 组装时插入分隔行：`render()` 里 `if (rows.length > 0) rows.push("")`（`transcript-container.ts:965`）；滚动回放路径 `#renderRange` 同样 `if (rows.length > 0) rows.push("")`（`transcript-container.ts:1223-1225`）；视口路径 `renderViewport` 亦同（`transcript-container.ts:592-596`）。
- 即：**每个块与前面的可见内容之间恰好一个空行**；块自身首尾的纯空白行不参与计数，因此不会叠加成两个。

17.3.8 里这条规则写得更直白（`pca/src/modes/components/transcript-container.ts:589-594` 的注释）：

> 「Every block is separated from preceding visible content by exactly one blank row — skipped when it opens the transcript or the prior row is already a plain blank (a fragment's own trailing pad), never doubling.」

`TranscriptBlock`（一个空 `Container` 子类）的用途就是**把多行当成一个块**，避免容器在它们之间插空行（`transcript-container.ts:1490`；17.3.8 同一处在 `transcript-container.ts:648-657`，注释说明这是给 IRC 卡片、file-mention 列表、带边框面板用的）。

**结论（对应 owner 的「空行有的有有的没有」）**：在 omp 里这是**一条统一规则**——块间恒为一空行；「看起来没有」的情况是块自己把首尾空白行当作内容渲染出来了（例如带背景的 padding 行被保留），或某类块被设计成贴着上一块（如块内子块用 `TranscriptBlock` 包起来）。**不是每个块各自决定**。

### 1.2 左侧标记与缩进

omp **没有**统一的一条「标记列」。它有两套机制，按块类型分：

**(a) 工具类块：状态图标 + 标题的单行 header（`renderStatusLine`）**

`tui/src/render/status-line.ts:32-54`：一行 = `图标 标题[: 描述][ [badge]][ · meta]`。图标来自 `formatStatusIcon`（`tui/src/render/render-utils.ts:277-300`），按状态取符号：

| 状态 | unicode 符号（`tui/src/theme/symbols.ts:392-402`） | 颜色 |
|---|---|---|
| success | `✔` | success |
| error | `✘` | error |
| warning | `⚠` | warning |
| info | `ⓘ` | accent |
| pending | `⏳` | muted |
| running | `⟳`（或 spinner 帧） | accent |
| aborted | `⏹` | error |
| done | `•` | success |

符号有三套 preset：`unicode` / `nerd` / `ascii`（`tui/src/theme/symbols.ts`，ascii 一套是 `[ok]`/`[!!]`/`[!]`/`[i]`/`[*]`/`[x]`/`[~]`…，`symbols.ts:1174+`）。`renderStatusLine` 里图标与标题之间是**一个空格**（`status-line.ts:38`），描述用 `: `，badge 用 `⟦…⟧`（`theme.format.bracketLeft/Right`），meta 用 ` · `（`theme.sep.dot`）。

**(b) 非工具块：没有统一标记，各自渲染**

- **用户消息**：`tui/src/chat/user-message.ts:174` 起——一个 `Markdown(text, 1, 1, …)`，即 **paddingX=1、paddingY=1**，带 `userMessageBg` 背景（`user-message.ts:165, 174-179`）。**没有** `> ` 之类的左侧标记；靠**整块背景色**区分。首行还带 OSC 133 shell-integration 标记（`user-message.ts:53-57, 351-352`，用于终端多路复用器识别 prompt 区）。live-steer 的消息在**第一行左上角**放一个 `*`（accent 色，`user-message.ts:299-302`）。
- **助手正文**：`Markdown(trimmed, 1, 0, …)`（`tui/src/chat/assistant-message.ts` 的 `#createMarkdown`，`assistant-message.ts:1337-1344`；正文处调用在 `assistant-message.ts:1843`）——**paddingX=1、paddingY=0**。无标记、无背景。
- **thinking**：同样 `Markdown(text, 1, 0, …)`，但 `color: thinkingText` 且 `italic: true`（`assistant-message.ts:1340-1343`）。18.10.4 新增了**可折叠的 head**：流式中显示 `Thinking…` + 计时 + tok/s（`assistant-message.ts:930-943`），结束后折叠成一行 `Thought for 12s`（`thoughtLabel`，`assistant-message.ts:50-55`；未见过流式则是 `Thought`）。折叠状态由 `collapsible`/`collapsed` 属性驱动（`assistant-message.ts:951-955`）。
- **工具调用/结果**：见 §2。
- **notice / 状态消息**：`StatusNotice`，`Text(message, 1, 0)` + 前置 `Spacer(1)`（`tui/src/chrome/status-notice.ts:40-42`），默认 dim 色（`pca/src/modes/utils/ui-helpers.ts:238-256`）。**无标记**。连续多条 notice 会**替换上一行**而不是追加（`ui-helpers.ts:247-251`）。
- **错误**：块内 `Text(theme.fg("error", "Error: …"), 1, 0)`（`assistant-message.ts` 的 `#appendErrorBlock`，定义在 `assistant-message.ts:1494-1510`，调用在 `assistant-message.ts:1917`；折叠时上限 `MAX_TRANSCRIPT_ERROR_ROWS = 8`，`assistant-message.ts:47`）。另有一个**钉在输入框上方的 banner**（`tui/src/overlays/error-banner.ts:26-51`）：`Spacer(1)` + `DynamicBorder(error)` + 首行 `✘ <第一行>`（粗体 error 色）+ 后续行 + 一行 dim 说明 + 底部 `DynamicBorder`。banner 是**双横线夹住的**，transcript 里的是**普通缩进行**。
- **压缩摘要**：一条**规则线**，不是标记行：`tui/src/chat/compaction-summary-message.ts:68-90` 画 `──── 📷 compacted · ctrl+o ────`（`theme.tree.horizontal` 做横线，label 居中，两侧按剩余宽度平分；宽度不够时退化成裸 label）。展开后是 `Box(1, 1, bg=customMessageBg)` 里的 Markdown（`compaction-summary-message.ts:180`）。18.10.4 的 label 按压缩方式变化（`remote-compacted` / `soft-compacted` / `snap-compacted`，`compaction-summary-message.ts:14-19`）。

### 1.3 背景（band）

`card_band` 决定条目是否铺背景（Radian 的对应物在 §6）：

- 用户消息 → `userMessageBg`。
- 工具卡片 → 按状态 `toolPendingBg` / `toolSuccessBg` / `toolErrorBg`（18.10.4 在 `tui/src/theme/dark.json:16-18, 39-41`；17.3.8 同一份文件在 `pca/src/modes/theme/dark.json`，行号相同）。
- 其余块无背景。

### 1.4 宽度

**块铺满终端宽度**。背景是**整行铺满**的（`applyBackgroundToLine` / `Box` 的 `pushRow` 补足到 `innerWidth`，`tui/src/components/box.ts:176-183`）。文字**不**铺满，是 `paddingX` 之后按宽度换行（`tui/src/components/text.ts:130-158`）。

### 1.5 上下留白（paddingY）

omp 用 `paddingY` 表达「块内上下各留一行」，且**只有带背景的块才留得住**——因为容器会把纯空白的首尾行剥掉（§1.1）。这一点 17.3.8 有明确注释（`pca/src/modes/components/tool-execution.ts:479-482`）：

> 「paddingY is 1 so background-tinted blocks (custom/extension tools and the generic fallback) get top/bottom breathing room. TranscriptContainer strips PLAIN-blank edges, so framed/minimal blocks (no bg set) drop these lines and keep their tight spacing — only tinted lines survive.」

即 **paddingY 与「块间空行」不是同一套机制**：块间空行由容器统一给（恒 1 行）；paddingY 是块**自己**要不要多留一行（只在有背景时才可见）。

---

## 2. 工具调用与结果的配对

### 2.1 一张卡片，结果填进同一张卡片

**不是**「调用一条、结果一条」，而是**每次调用一张卡片，结果到达后填进这张卡片**。证据（18.10.4，与 17.3.8 同构）：

- `pca`/`tui` 的 `chat-transcript-builder.ts` 用 `#pendingTools: Map<toolCallId, ToolExecutionComponent>`：遇到 `toolCall` 建组件并登记（`tui/src/chat/chat-transcript-builder.ts:519`），遇到 `toolResult` 用 `message.toolCallId` 取回**同一个组件**再 `updateResult`（`chat-transcript-builder.ts:536-554`）。
- `ToolExecutionComponent.updateArgs` / `updateResult`（`tui/src/chat/tool-execution.ts:229-231, 527, 627`）——同一个组件的两个阶段：先有调用参数，后有结果。
- 组件的 `#rebuildDisplay` 依次加「call 组件」和「result 组件」到同一个 `contentBox`（17.3.8 `pca/src/modes/components/tool-execution.ts:1082-1167`；18.10.4 走 `ToolCard`，见 §4）。

### 2.2 一批多个调用怎么排

**按 assistant 消息里 toolCall 的出现顺序**，每个调用一张卡片，结果填回各自的卡片。批内**调用与结果不交错**——因为同一 assistant 消息里的所有 toolCall 在构建时就全部建卡（`chat-transcript-builder.ts:468-521` 的循环），结果稍后按 `toolCallId` 回填。**所以从 transcript 上看是「卡片、卡片、卡片」，不是「调用、结果、调用、结果」。**

assistant 消息里**夹在 toolCall 之间的文本**会被切成独立的助手块，插在对应卡片之后（`splitAssistantMessageToolTimeline` 把消息切成 `beforeTools` + 每个 toolCall 之后的片段，`tui/src/chat/transcript-render-helpers.ts`；17.3.8 在 `pca/src/modes/utils/transcript-render-helpers.ts:148-188`）。

**例外与特例**：
- `read` 工具的一组调用会**合并成一张** `ReadToolGroupComponent`（`readArgsCollapseIntoGroup`，`chat-transcript-builder.ts:471-489`）。
- `todo` / `hub` 的「快照」卡片是**可替换的**：后续同类调用会把前一张**撤掉**（`isDisplaceableBlock` / `canBeDisplacedBy`，`tui/src/chat/tool-execution.ts:881-890`；builder 里 `#resolveTodoSnapshot` / `#resolveWaitingPoll`）。撤换只在卡片**尚未进入 scrollback** 时进行（`isBlockUncommitted`）。
- 一次 `write`/`patch` 改多个文件时，**每个文件一个 `Box`**，之间用 `Spacer(1)`（17.3.8 `tool-execution.ts:1187-1219`）。

### 2.3 运行中 / 完成 / 失败

- **状态来源**：`isTranscriptBlockFinalized()`（`tool-execution.ts` 同名列）——结果还没到、或前台工具在流式输出部分结果时返回 `false`，容器就把这张卡留在「可重绘的 live 区」，不提交进原生 scrollback；结果到齐后返回 `true` 冻结。
- **视觉**：header 的图标按状态变（§1.2 表）。卡片背景按状态变（pending / success / error，§1.3）。运行中显示 spinner（`formatStatusIcon("running", …, spinnerFrame)`）。
- **失败**：`isError` 的卡片用 error 背景 + `✘`，且**错误一律展开**（Radian 的 ADR 0028 R4 也定了同样的默认）。
- **取消**：`resolveExecutionStatus(exitCode, cancelled)` 先看 `cancelled`（`tui/src/chat/execution-shared.ts`；17.3.8 `pca/src/modes/components/execution-shared.ts:97-101`），`bash`/`eval` 的 footer 显示 `(cancelled)`，失败显示 `(exit N)`（`buildStatusFooter`，17.3.8 `execution-shared.ts:67-91`）。

---

## 3. 折叠与预览

**折叠状态是全局的，不是逐条的光标**（与 Radian ADR 0028 R4 首版的选择一致）。

- 一个 `setExpanded(expanded)` 遍历所有可展开组件：`ChatTranscriptBuilder.setExpanded`（`tui/src/chat/chat-transcript-builder.ts:146-149` 的 `#expandables` 数组 + `#trackExpandable`，`chat-transcript-builder.ts:108, 201-204`）。绑定 `Ctrl+O`（Radian 同）。
- **折叠时显示什么**：
  - 工具卡片：header 一行 + **尾部 N 行**预览。`PREVIEW_LINES = 20`（`tui/src/chat/execution-shared.ts:26`；17.3.8 同值在 `bash-execution.ts:29`），Radian 用 5 行（ADR 0028 R4 的 `tui.display.tool_preview_lines`）。
  - 折叠预览用**视觉行截断**（按终端宽度重算），不是按逻辑行数：`createCollapsedPreview` → `truncateToVisualLines(previewText, previewLines, width, 1)`（17.3.8 `pca/src/modes/components/execution-shared.ts:55-60`）。
  - 被藏起来的行数会写成一行提示：`… N more lines (ctrl+o to expand)`（17.3.8 `execution-shared.ts:77-79`）。
  - thinking：折叠成一行（18.10.4 是 `Thought for 12s`；Radian 是 `▸ thinking (N lines)`）。
  - 压缩摘要：折叠成一条规则线，`ctrl+o` 展开。
  - 合成的用户输入（如 advisor 的 `Session update` 回放）：折叠成一行 dim 摘要 `<label> · <大小> · <N> lines · ctrl+o`，**并且不构建 Markdown**（懒构建，`user-message.ts` 的 `CollapsedSyntheticMessageComponent`；17.3.8 `pca/src/modes/components/user-message.ts:99-145`）。这是为了防冷开时几百 KiB 的 Markdown 卡住 TUI。
- **展开入口**：`ctrl+o`（`expandKeyHint()`，可重绑）。提示文字统一由 `formatExpandHint` 生成（`tui/src/render/render-utils.ts:306-310`）。
- 18.10.4 还给了**逐块**折叠的能力（`ToolCard` 的 `collapsible`/`collapsed`、thinking 的 `#thinkingCollapsed` 按 content index 记），但全局 `Ctrl+O` 仍是主入口。

---

## 4. 有没有一套统一的「块」抽象？

**有，而且是分层的两套。**

### 4.1 第一层：生命周期抽象 `ChatBlock`（两版都有）

`tui/src/chrome/chat-block.ts`（18.10.4，全文 111 行；17.3.8 在 `pca/src/modes/components/chat-block.ts`，逐行相同）：

- `abstract class ChatBlock extends Container`，带 `mount/finish/dispose`、`onMount/onCleanup`（类 React/Svelte 的生命周期）、`requestRender()`。
- 关键契约：**未完成的块 `isTranscriptBlockFinalized() === false`**，容器据此把它留在可重绘的 live 区；`finish()`/`dispose()` 后返回 `true` 冻结。
- 生产者「返回一个块，让宿主挂载」，而不是直接去戳 `chatContainer`。

这一层管**生命周期**（谁在动、什么时候冻结），**不管排版**。

### 4.2 第二层：排版抽象（18.10.4 新增 `ToolCard`，17.3.8 只有零散原语）

**17.3.8**：只有共享原语——`renderOutputBlock`（带圆角边框的输出块，`tui`→ 当时在 `pca/src/tui/output-block.ts`）和 `renderStatusLine`（`pca/src/tui/status-line.ts`）。每个工具渲染器自己拼。

**18.10.4**：抽出 `ToolCard`（`tui/src/render/tool-card.ts`，424 行），这是**真正意义上的「统一块」**：

- 固定拓扑：`ToolCardVariant = "framed" | "plain"`（`tool-card.ts:26-27`）——带边框的卡片自己画边框；plain 卡片内联流动。
- 生命周期阶段：`ToolCardPhase = "pending" | "running" | "partial" | "success" | "warning" | "error" | "info"`（`tool-card.ts:23-24`），映射到输出框的视觉状态（`toolCardState`，`tool-card.ts:80-85`）。
- 快照结构：`ToolCardSnapshot { header | status, headerMeta, phase, body, sections, footer, applyBg, contentPaddingLeft/Right, borderColor }`（`tool-card.ts:39-54`）。header 要么给现成字符串，要么给**结构化的 `StatusLineOptions`**（由共享的 status-line 原语渲染）。
- 统一的 padding 入口：`ToolCardOptions.paddingX / paddingY / ignoreTight`（`tool-card.ts:65-78`），内部对 framed 用 `outputBlockContentWidth(width, left, right)` 算内容宽度（`tool-card.ts:159-172`），对 plain 用 `Text`（`tool-card.ts:151-152`）。
- 两个构造器：`framedToolCard` / `plainToolCard`（`tool-card.ts:409-424`）。
- 与容器契约的接口：`markFramedBlockComponent` / `isFramedBlockComponent`（`tui/src/render/output-block.ts`，18.10.4 版）——**自绘边框的块被标记后，外层就不再给它加 padding/背景**（避免双重内边距）。17.3.8 里同一个想法用「有没有 border」隐式判断（`pca/src/modes/components/tool-execution.ts:1170-1172`：`customFramed` → `setPaddingX(0)` 且 `setBgFn(undefined)`）。

**所以对 owner 的问题 4 的回答**：omp **有**统一的块抽象，但要分清两层——`ChatBlock` 管生命周期；`ToolCard`（18.10.4）管工具类卡片的排版。**非工具块（用户消息、助手正文、thinking、notice）没有走 `ToolCard`**，它们各自 new 一个 `Markdown`/`Text` 并各自传 paddingX/paddingY。也就是说：**omp 的统一性是「工具卡片内部统一 + 容器统一管块间距」，不是「所有块一套 gutter」**。这正是为什么 omp 看起来整齐：**间距统一由容器保证（§1.1），而标记/缩进只在工具卡片这一族里统一**。

### 4.3 底层的组件原语

`tui/src/components/box.ts`（`Box(paddingX, paddingY, bgFn, border)`，`box.ts:56-61`）、`tui/src/components/spacer.ts`（`Spacer(n)`）、`tui/src/components/text.ts`（`Text(text, paddingX, paddingY, bgFn)`，`text.ts:46-51`）。`Markdown` 的签名是 `(text, paddingX, paddingY, theme, style?, codeBlockIndent=2)`（`tui/src/components/markdown.ts`）。所有块最终都是这些原语的组合。

---

## 5. `!` 运行命令

### 5.1 omp（18.10.4，与 17.3.8 行为一致）

**识别**：输入框里以 `!` 开头的行。`pca/src/modes/controllers/input-controller.ts:1140-1155`：

```
if (text.startsWith("!")) {
  const isExcluded = text.startsWith("!!");
  const command = isExcluded ? text.slice(2).trim() : text.slice(1).trim();
  ...
  await this.ctx.handleBashCommand(command, isExcluded);
```

- **`!cmd`**：跑命令，输出**进入模型上下文**。
- **`!!cmd`**：跑命令，输出**不进**模型上下文（`excludeFromContext`）。**有 `!!` 变体**。
- 对照的 Python 版本：`$code` / `$$code`（`input-controller.ts:1163-1173`，`parsePythonCommandInput` 在 `input-controller.ts:203-211`，`prefixLength === 2` 即 excluded）。
- 另有一个 UI 模式标志 `isBashMode`（输入框边框变色，`input-controller.ts` 里设置；17.3.8 `pca/src/modes/interactive-mode.ts:540`），欢迎页也提示「`!` to run bash」（17.3.8 `pca/src/modes/components/welcome.ts:336`）。

**是否进上下文**：由 `excludeFromContext` 决定，落在消息类型 `BashExecutionMessage` 上：

- `pca/src/session/messages.ts:902-916` 的 `bashExecutionToText`：转成 user 消息文本 `Ran \`cmd\`` + 三反引号包住的输出 + `(command cancelled)` 或 `Command exited with code N` + 输出截断提示。
- `convertOne`（`pca/src/session/messages.ts:1061-1074`）：`if (m.excludeFromContext) return [];` 否则包成 `role: "user"`、`attribution: "user"` 的一条消息。
- 即 **`!` 的结果在模型看来是一条「用户消息」**（不是工具结果）。

**走不走权限与沙箱**：**不走审批，也没有 omp 自己的沙箱层**。核实过：

- `handleBashCommand` → `#executeBashCommand`（`pca/src/modes/controllers/command-controller.ts:1798-1845`）→ `session.executeBash(command, onChunk, { excludeFromContext, useUserShell: true, pty: {...} })`。
- `session/bash-runner.ts:71-116` 的 `executeBash` 直接调 `exec/bash-executor.ts` 的 `executeBash`；**整条路径没有 `resolveApproval`**（`grep approval` 在 `bash-runner.ts` / `bash-executor.ts` 里**零命中**）。审批只用于**模型发起的工具调用**（`pca/src/tools/approval.ts:205` `resolveApproval`，调用方是 `modes/controllers/event-controller.ts`、`extensibility/extensions/wrapper.ts`、`cursor.ts`）。
- **唯一的外部干预点**是扩展 hook：`user_bash` 事件（`pca/src/extensibility/extensions/types.ts` 的 `UserBashEvent { command, excludeFromContext, cwd }`，注释原文「Fired when user executes a bash command via ! or !! prefix」）。hook 可以返回一个结果直接顶替执行（`bash-runner.ts:80-92`），也可以拒绝——**但那是扩展的事，不是内建审批**。
- **沙箱**：`grep sandbox` 在 `pca/src` 的命中都是路径沙箱/artifact sandbox/plan-mode 之类，**没有**「把 `!` 命令关进 OS 沙箱」的机制。omp 的模型工具 `bash` 也没有走 sandbox-exec/bwrap 这类强制边界（本次未逐行核实 `tools/bash.ts` 的完整执行路径，**未核实**）。
- **`useUserShell: true`**：`!` 走**用户配置的 shell**（`bash-executor.ts` 里 `resolveUserShellConfig`），不是内建工具用的 shell；18.10.4 还新增了 **PTY 模式**（`pty: {...}`，注释说「User-shell zsh/fish `!` commands run on a headless PTY; raw bytes render through the component's vterm replay (color-safe)」，`command-controller.ts:1841-1843`）。所以 `!` 能跑交互式命令、能保留颜色，而模型工具不行。
- **cwd 会持久**：`isPersistentShellCdCommand(command)` 为真时（`exec/bash-executor.ts:235`），命令成功后 omp 会把**会话工作目录**移到结果里的 `workingDir`（`#applyBashResultCwd`，`command-controller.ts:1200-1216`；17.3.8 `command-controller.ts:1200-1216`）。即 `!cd /tmp` 会真的改变后续会话的 cwd。流式输出中若命令要 `cd`，omp 会先警告「Wait for the current response to finish…」（`command-controller.ts:1799-1804`）。

**在 transcript 里怎么显示**：`BashExecutionComponent`（`tui/src/chat/bash-execution.ts`，17.3.8 在 `pca/src/modes/components/bash-execution.ts`）：

- **上下各一条横线**把整块夹住：`buildExecutionFrame` 加 `DynamicBorder` → 内容 → `DynamicBorder`（`tui/src/chat/execution-shared.ts:52-74`）。
- **边框颜色 = 是否进上下文**：`const colorKey = excludeFromContext ? "dim" : "bashMode"`（`tui/src/chat/bash-execution.ts:102`；17.3.8 `pca/src/modes/components/bash-execution.ts:56` 注释原文「Use dim border for excluded-from-context commands (!! prefix)」）。`bashMode` 在 dark 主题里是 `cyan`（`tui/src/theme/dark.json:73`）。**所以 `!` 是青色框，`!!` 是暗灰框——一眼能区分**。
- **首行**：`$ <命令>`（粗体、同边框色），paddingX=1（`bash-execution.ts:120`）。
- **运行中**：一行 spinner `Running… (esc to cancel)`（`execution-shared.ts:64-70`；18.10.4 把 esc 换成 `interruptKey()`）。
- **完成后**：footer 显示 `… N more lines (ctrl+o to expand)` / `(cancelled)` / `(exit N)` / 截断提示（`buildStatusFooter`）。
- 折叠时显示**尾部 20 行**（`PREVIEW_LINES = 20`）。

**运行中能否取消**：**能**。`bash-runner.ts` 持有 `AbortController` 集合（`bash-runner.ts:57, 112-128`），`abort()` 逐个 abort（`bash-runner.ts:160-162`），`isRunning` 反映是否有在跑的（`bash-runner.ts:165-167`）。UI 上提示 `esc to cancel`，且**同时只允许一条** `!` 命令：已有在跑时再提交会警告「A bash command is already running. Press <interrupt> to cancel it first.」并**保留输入行**（`input-controller.ts:1144-1150`）。

### 5.2 pi（1.2.0）

pi 只提供编译后的 `dist/`，所以结论主要来自随包文档：

- `docs/usage.md:70-76`：**「Prefix a command with `!` to run it and include its output in the conversation」**；**「Use `!!` when you want to run a command without sending its output to the model.」** —— **语义与 omp 完全一致**（`!` 进上下文，`!!` 不进）。
- `docs/shell-aliases.md:40`：`shellCommandPrefix` 会**同时**作用在内建 `bash` 工具与用户的 `!`/`!!` 命令上——即 pi 里 `!` 命令也走「用户 shell 配置」这条路。
- `docs/environment-variables.md:49`：pi 注入给 LLM 的 `bash`/`powershell` 工具的环境变量**不**注入用户的 `!`/`!!` 命令。这条很有信息量：**pi 明确把「用户亲手跑的命令」与「模型跑的命令」分成两套环境**。
- 是否走权限：pi 的 `dist/` 里 `!` 的处理路径**未逐行核实**（**未核实**）。文档层面只说「run it」；从 `docs/usage.md` 的语气看它被当作**用户自己的终端动作**，不是模型工具调用。

### 5.3 小结（两版对照）

| 问题 | omp 18.10.4 | pi 1.2.0 |
|---|---|---|
| `!cmd` 进上下文 | 是（包成 user 消息） | 是（文档明说） |
| `!!cmd` 不进上下文 | 有（dim 边框区分） | 有（文档明说） |
| 走审批 | **否**（只有扩展 `user_bash` hook 可干预） | 未核实 |
| 走 OS 沙箱 | **否**（无沙箱层） | 未核实 |
| 走用户 shell | 是（`useUserShell: true`，18.10.4 还有 PTY） | 是（`shellCommandPrefix` 同时作用于两者） |
| transcript 显示 | 上下横线夹住的块，`$ cmd` 首行；颜色区分进/不进上下文 | 未核实（无源码） |
| 运行中取消 | 是（esc；同时只允许一条） | 未核实 |

---

## 6. 对 Radian 的建议

### 6.1 (a) 一套 Radian 的块排版规则

先指出**Radian 现状的机制性原因**（这决定了改动量）：

Radian 现在**没有**「容器统一给块间空行」这一层。`crates/radian-tui/src/render.rs:517-527` 的 `transcript_lines` 是：

```rust
entries.iter().flat_map(|entry| entry_lines(entry, modes, width, theme)).collect()
```

即**条目首尾相接、没有插入分隔行**；`visible_lines`（`render.rs:533+`）同样只做窗口切片。**块之间有没有空行，完全取决于每个条目自己的 body 首尾有没有空行**：

- 用户条目**自带**上下各一个 padding 行（`blocks.rs:812-817`：`vec![BodyLine::pad(0)]` + 正文 + `BodyLine::pad(end)`）——所以用户块上下有空白。
- 助手条目**没有**（`blocks.rs:819+` 直接开始放段落）。
- 工具卡片看折叠状态和预览行数（`blocks.rs:882-946`）。
- 所以「有的有有的没有」**不是标记不一致，是缺少统一的分隔层**。

同样地，**标记列**现在是 `marker_prefix` 一张表（`render.rs:177-194`）：

| 条目 | 前缀 | 宽度 |
|---|---|---|
| User | `"> "` | 2 |
| Assistant | `"  "` | 2 |
| Reasoning | `"~ "` | 2 |
| ToolCall | `"-> "` | 3 |
| ToolResult | `"<- "` | 3 |
| Notice | `"* "` | 2 |
| Warning | `"! "` | 2 |
| Banner / Startup | `"  "` | 2 |
| Tip | `"Tip: "` | 5 |
| Compaction | `""` | 0 |

宽度 **2 / 3 / 5 / 0 不等**，这正是 owner 截图里「标记与缩进各不相同」的来源。`render_row` 里正文对齐用 `" ".repeat(prefix.width())`（`render.rs:370-376`），所以**同一块内**是齐的，但**跨块**不齐。

建议如下（每条都标出处；改动按「先小后大」排序）：

**规则 1（最重要）：把「块间空行」收进渲染层，条目自己不再管上下留白。**

- 做法：在 `transcript_lines` / `visible_lines` 的条目循环之间插入恰好一个空行（`rows` 非空且上一条不是空行时才插），并**剥掉每个条目 body 首尾的纯空白行**。这就是 omp 的 `trimBlankEdges` + `rows.push("")`（`transcript-container.ts:203-210, 965`）。
- 后果（对照截图逐条）：
  - 用户块：**保留**它自带的上下 padding 行（`blocks.rs:813, 816`）——因为那两行**带背景**（§规则 1 的注意），是它背景形状的一部分，与 omp 一致；容器**不**再为它额外插空行（判据：上一条的末行若已是有 band 的空白行，就不插）。**外观不变**，但从此与其它块同规则。
  - 助手块、notice、工具卡片：**获得**统一的一行空白（现在是「有时没有」）。
  - 「同一轮内 vs 跨轮」不再需要区分：**一律一行**。这与 omp 一致（omp 不区分轮内轮外）。
- 注意：`BodyLine::pad` 还承担**带背景行的延续**（用户块/工具卡片的背景要铺满整行，`render.rs:364-372` 的 band 分支：`Some(band) => Line::from(Span::styled(" ".repeat(width), band))`）。若照 omp 的办法剥空白，必须**只剥「无背景的纯空白行」**——omp 的 `isPlainBlank` 正是「空且无 ANSI」（`transcript-container.ts:164-166`）。所以 Radian 的判据应是「空行且 `band.is_none()`」，否则会把用户块的背景带切掉。
  - **但这里有个与 omp 不同的取舍要点**：omp 的用户块上下留白**恰恰是靠带背景的 paddingY 行**实现的（§1.1 里 `#renderEmptyPaddingLines` 的引用）——它**不是**靠块间空行。所以 omp 的用户块看起来「上下有空白」，实际上是**背景带延伸了一行**，而那行是**有颜色的**、不会被剥掉。Radian 现在的 `BodyLine::pad` + band 分支（`render.rs:364-372`）是**同一个机制**。因此规则 1 的正确做法是：**用户块保留它自己的 banded pad 行（那是它背景设计的一部分），只把「无 band 的纯空白行」交给容器统一处理**。这样既统一了间距，又不破坏用户块的背景形状。
- 代价：`entry_line_count`（`render.rs:510-512` → `blocks::line_count_cached`，`blocks.rs:696+`）与 `visible_lines` 的行数记账都要**加上分隔行**，否则虚拟滚动会错位。这是这条建议唯一的风险点，建议做成一个共享的 `block_gap(prev, next) -> usize` 纯函数，计数与绘制都调它。

**规则 2：标记列统一宽度（一条 2 格的 gutter），标记字符只在里面变。**

- 现状宽度 2/3/5/0 不等（上表）。建议**全部对齐到 2 格**（Radian 现在的主流已经是 2 格：`"> "`、`"  "`、`"~ "`、`"* "`、`"! "`、`"  "` 都是 2），只改三个：
  - `ToolCall`：`"-> "` → `"->"`（2 格）或 `"» "`。**建议 `"->"`**，最小改动、保留「调用」的语义。
  - `ToolResult`：`"<- "` → `"<-"`。
  - `Tip`：`"Tip: "`（5 格）→ 改成和其它块一样 2 格前缀 + 把 `Tip:` 当**内容**的第一个词（Radian 已经在 `split_label` 里有「标签 + 正文」的两级样式，`render.rs:330+`；`Tip:` 本来就按 label 处理）。
  - `Compaction`（0 格）保持 0：它是**规则线**，不是标记行——与 omp 的压缩摘要一致（`compaction-summary-message.ts:68-90`）。
- 这样**所有块的正文都从第 3 列开始**，标记列成为真正的「gutter」。
- 与 omp 的关系：omp **没有**做这件事（它的非工具块根本没有标记列，工具块用 header 图标）。所以这条是**Radian 自己的设计**，不是抄 omp。**但值得做**——Radian 有标记列而 omp 没有，Radian 的标记列只要对齐就会比现状好得多。若 owner 更想「像 omp」，另一条路是**取消标记列**、改用背景带 + header 图标；代价是改动更大，且会丢掉 Radian 现在一眼可辨的块类型。**建议保留标记列、只对齐宽度。**

**规则 3：用户块与工具卡片的背景带保留，其它块不加背景。**

- 现状已有：`card_band`（`render.rs:242-259`）给用户块 `user` 背景、工具卡片按状态 `tool_pending_bg`/`tool_success_bg`/`tool_error_bg`。
- 这与 omp 一致（omp 同样只给这两类铺背景，§1.3）。**建议不改**，只确保规则 1 的「剥空白」不误伤 band 行。

**规则 4：工具调用与结果保持「两次条目」，但**视觉上成对**。**

- Radian 现在是 `ToolCall` 和 `ToolResult` 两个**独立条目**（两个 `EntryKind`，`app.rs:106-107`）。omp 是**一张卡片**（§2.1）。
- **不建议**现在改成 omp 的一张卡片：那要动条目模型、journal 投影和 `blocks.rs` 的卡片布局，是 ADR 0028 R4 之外的重构。
- **建议**做视觉上的成对：
  - 让 `ToolResult` 的标记缩进比 `ToolCall` 多一格（例如调用 `"->"`、结果 `"  <-"`）——用**缩进**表达从属关系，而不是靠两个都顶格的前缀。这正好利用规则 2 腾出的 gutter。
  - 保证调用与结果**之间不插空行**（规则 1 的例外：同一次调用的 call/result 视作一个块）。这与 owner 说的「调用和结果分开了」直接对应：**保持两条、但贴在一起**。
  - 依据：omp 的 `TranscriptBlock` 注释正是这个意思——「把一组兄弟行归成一个块，让容器按一个块给间距」（`transcript-container.ts:1490` 上方注释；17.3.8 `transcript-container.ts:648-657`）。
- 运行中/完成/失败的表示：现状已有 `is_error` 与 `tool_pending_bg`（`render.rs:246-251`）。建议补一条：**运行中的调用显示 spinner**（Radian 已有 `SPINNER`，`blocks.rs:1026`），完成后换成状态符号。omp 用 `⟳`/`✔`/`✘`（`symbols.ts:392-402`），Radian 可直接沿用同一组（它们是 unicode 通用符号，不必抄主题）。

**规则 5：折叠占位行沿用现有形状，但对齐到 gutter。**

- 现状：`▸ thinking (N lines)`（`blocks.rs:1021-1030`），带 spinner 当 live（`blocks.rs:1017` 定义 `SPINNER`）。这与 omp 的 `Thought for 12s` 不同但**同样合理**（Radian 显示行数，omp 显示耗时）。**不建议改文字**。
- 只建议：占位行的 `▸` 落在 gutter 的**第 2 格**（即 `"▸ "` 两格），使展开/折叠时正文列不跳。omp 的做法是折叠 head 与 body 同属一个 section，宽度一致（`assistant-message.ts:945-962`）。

**规则 6：压缩摘要保持规则线。**

- 现状 `EntryKind::Compaction` 前缀为空（`render.rs:186`），是规则线。与 omp 的 `──── 📷 compacted · ctrl+o ────`（`compaction-summary-message.ts:68-90`）同构。**不改**，只在规则 1 生效后确认它上下各一行空白。

**汇总：对照截图逐条会怎么变**

| 截图里的现象 | 现在的原因 | 规则 1-6 之后 |
|---|---|---|
| thinking `~ ▸` | `Reasoning` 前缀 2 格，占位行自带 `▸ ` | 前缀仍 2 格；`▸` 落 gutter 第 2 格，正文列与展开时一致 |
| 助手正文缩进两格 | `Assistant` 前缀 `"  "` | 不变（已是 2 格） |
| 工具调用 `-> ` | 前缀 3 格 | 收成 2 格 `"->"`，正文与其它块同列 |
| 结果 `<- ` | 前缀 3 格 | 收成 2 格并**多缩一格**，视觉上从属调用 |
| notice `* ` | 前缀 2 格 | 不变 |
| 标记与缩进各不相同 | 宽度 2/3/5/0 混用 | 全部 2 格（Compaction 规则线除外） |
| 块间空行有的有有的没有 | **没有容器分隔层**，靠条目自带 padding | 容器统一给一行；用户块**带背景的** pad 行保留（不重复插） |
| 调用与结果分开 | 两个独立条目 + 无成对规则 | 仍两条，但**紧贴**且结果缩进，视觉成对 |

### 6.2 (b) `!` 命令在 Radian 的形状

**先给结论：建议做 `!cmd` / `!!cmd`，语义与 omp、pi 完全一致；并且它必须走沙箱、但不走审批。**

**(1) 形状与语义**

- `!cmd`：跑命令，输出**进入模型上下文**。`!!cmd`：跑命令，输出**不进上下文**。出处：omp `input-controller.ts:1140-1155`（`isExcluded = text.startsWith("!!")`）；pi `docs/usage.md:70-76`（文档明说）。**两个独立实现都这么做，这是事实标准。**
- 进上下文时的**形状**：omp 把它包成一条 `role: "user"` 的消息，文本是 `Ran \`cmd\`` + 三反引号包住的输出 + 状态尾巴（`messages.ts:902-916, 1061-1071`）。**建议 Radian 照抄这个形状**，理由是它让模型清楚地知道「这是用户亲手跑的、不是你的工具调用」，同时把命令与输出都放进一条 user 消息里，不需要新的消息类型。
- **`!` 的命令字符串本身也进上下文**（在 `Ran \`cmd\`` 里）。这一点要明确写进文档，否则用户会以为只有输出进去了。
- 转录里的显示：**建议照 omp 的两条横线夹块**（`buildExecutionFrame`，`execution-shared.ts:52-74`）+ 首行 `$ cmd` + **颜色区分是否进上下文**（omp：`!` 用 `bashMode` 青色、`!!` 用 `dim`；`bash-execution.ts:102`）。Radian 的主题已有 `notice`/`hint` 等角色，**建议 `!` 用 `tool_call` 角色、`!!` 用 `hint`**，理由：Radian 的 `tool_call` 已经是「命令」的颜色（`render.rs:201`）。
- 运行中可取消：**建议支持，且同时只允许一条**。omp 的 UI 是 `esc to cancel` + 已在跑时警告并**保留输入行**（`input-controller.ts:1144-1150`）。Radian 已有 Esc 中断的机制，复用它。
- **cwd 持久**：omp 的 `!cd /tmp` 会改会话 cwd（`command-controller.ts:1200-1216`）。**建议 Radian v1 不做**（理由：它会改动 workspace 投影与会话状态，是另一条语义；且 omp 自己也为此加了「流式输出中不许 cd」的警告）。**建议**在命令要 `cd` 时把它当普通命令跑（子进程内生效、不影响会话），并在文档里说明这个差异。

**(2) 来源与权限：按 ADR 0012，用户在输入框打的 `!` 是 `User` 来源**

- ADR 0011 决定 4 写定：**权限层把 `source == Some(User)` 当作人类输入的唯一依据**；`None` 与任何非 `User` 值都不具备 `User` 语义（`docs/adr/0011-durable-inbox-semantics.md:37`）。
- ADR 0005 G2：`InputSource` 无默认值，`None` **不等于** `User`（`docs/adr/0005-host-message-identity.md:33`）。
- 所以：**用户在输入框亲手打的 `!cmd`，其来源必须是 `InputSource::User`**。它**不是**模型发起的工具调用，不能借 `ToolResult` 的身份绕过来源判断；也**不能**被标成 `System`（那会丢掉用户语义，ADR 0018 的摘要层也会漏收，`docs/adr/0018-model-catalog-and-auto-compaction.md:67`）。
- **但「来源是 `User`」不等于「跳过沙箱」**。ADR 0012 P1 写死了三层的分工（`docs/adr/0012-permission-sources.md:23`）：
  > 「**`SandboxMode` = 实际执行边界**（进程/文件/网络能碰什么的强制约束）。**不能由 approval 扩大**：审批只能做『这道命令要不要问人』，不能把一个沙箱拒绝的操作变成放行。」
- 这条对 `!` 的直接含义：**`!cmd` 必须落在当前档位的沙箱里**——`safe` 档下**没有 shell**（`docs/adr/0012-permission-sources.md:27`：「`safe`（只读内建工作区/git 工具；**无 shell**、无网络…）」），所以 **`safe` 档下 `!` 应当不可用并明确报错**，而不是「因为是人打的所以放行」。这正是 ADR 0012 P5 的硬要求：「不用审批冒充 sandbox」（`0012:58`）。
- ADR 0016 决定 3：`workspace` 档**开网**（`docs/adr/0016-workspace-tier-network-and-devices.md:17`），并**放开 pty**（决定 4，`0016:18`）。这对 `!` 是好消息：**omp 18.10.4 的 `!` 走 PTY**（`command-controller.ts:1841-1843`，为了 zsh/fish 的交互与颜色），而 Radian 的 `workspace` 档已实测能让 `os.openpty()` 成功（`0016:35-37` 的实施要求 1）。**所以 Radian 的 `!` 在 `workspace` 档可以支持 PTY**，且不必为它单独开沙箱口子。
- **`full` 档**：ADR 0016 决定 6，`full` 无沙箱、**必须显式选择、从不是默认**（`0016:22`）。`!` 在 `full` 下就是裸跑。

**(3) 走不走审批？建议：不走（与 omp/pi 一致），但要有审计**

- omp：`!` **完全不走审批**（§5.1，`bash-runner.ts`/`bash-executor.ts` 里 `grep approval` 零命中）。pi：文档层面也把 `!` 当用户自己的动作（**未核实**其代码路径）。
- 建议理由：审批的意义是「**模型**要动你的机器，先问人」（ADR 0012 P1：`ApprovalPolicy` = 「匹配规则的动作是否暂停要求人类确认」）。**人自己打的命令再问一次人是纯摩擦**——人已经用「打字 + 回车」表达了意图。
- 但**审计要有**。ADR 0012 P6 已定：每次批准/拒绝写 durable `PermissionResolved`（`0012:62`），而 ADR 0002 E2 明确「不单独记录审批决定」（`docs/adr/0002-tool-execution-markers.md:17-19`）。`!` 不是审批，所以不产生 `PermissionResolved`。**建议**：`!` 的执行走**同一条 journal 路径**（等价于一次工具执行：`ToolExecutionStarted`/`Settled` 或一条专门的消息记录），这样「谁在什么时候跑了什么」在 journal 里可查。**这是本建议里最需要 拍板的一点**（见「待核实 / 待决定」）。
- **一个必须堵的口子**：ADR 0016 的实施要求 2 已警告——**允许对终端设备 `ioctl` 后必须堵住 `TIOCSTI`**（`0016:39-47`：沙箱内命令可能用 `TIOCSTI` 往终端输入里塞按键；原文「一旦允许对终端设备 `ioctl`，沙箱内命令可能用 `TIOCSTI` 往终端的**输入**里塞按键」，且要求「逃逸矩阵加一条：沙箱内对一个**沙箱外持有的终端**做 `TIOCSTI`，必须失败」）。`!` 走 PTY 意味着这条**从「可能的」变成「必然相关」**：PTY 路径必须一并满足 ADR 0016 实施要求 2，否则 `!` 会成为一个注入通道。**这条优先级最高。**

**(4) 解析位置**

- Radian 现在 `command.rs:761` 的 `parse` 只认 `/` 开头的命令（`Some("/help")` 等），`None => Outcome::Message`。**`!` 需要在这个 match 之前分流**（与 omp 一样，在输入控制器里先判 `!`，再判 `/`；omp 的 `!` 分支在 `/` 之后但都在「当成消息发送」之前，`input-controller.ts:1140-1175`）。
- **建议**：在 `command.rs` 加 `Outcome::Bash { command: String, exclude_from_context: bool }`（与现有 `Outcome` 枚举同风格，`command.rs:52-93`），解析放在 `parse` 的最前面（`!` 优先于 `/`——因为 `!/bin/sh` 这类命令可能含 `/`）。执行由 loop 负责（ADR 0023 U3 的分工：`update` 不做 I/O）。
- 输入框提示：omp 的欢迎页写「`!` to run bash」（`welcome.ts:336`）。**建议** Radian 的 `Tip:` 行或 `/help` 里加一行同样的话。

### 6.3 建议的落地顺序

1. **规则 1（容器管块间距）**——它单独就能消掉 owner 说的「空行有的有有的没有」，且是后面几条的地基（先有 gutter 再谈对齐）。风险集中在行数记账（`line_count_cached` / `visible_lines`），建议先写一个纯函数 + 单测。
2. **规则 2 + 4（标记列对齐 2 格、结果缩进从属）**——改动小、视觉收益最大，直接对应截图里的「标记与缩进各不相同」。
3. **规则 5（占位行对齐）**——一行的事。
4. **`!` 命令**——建议**先只做 `!cmd` 与 `!!cmd` 的语义 + 沙箱内执行 + 转录显示 + Esc 取消**，`cd` 持久化与 PTY 留给后续切片；**但 `TIOCSTI`（ADR 0016 实施要求 2）必须在 PTY 上线之前堵住**。

---

## 7. 未核实与待决定

**未核实**：

1. pi 1.2.0 的 `!` 代码路径（`dist/` 里未逐行定位到 `!` 的分流；随包只给了 `docs/` 的说明）。「pi 是否走审批/沙箱」**未核实**。
2. omp 的 `bash` **模型工具**是否走任何 OS 沙箱。本次只核实了 `!` 这条用户路径**没有**审批与沙箱层；`pca/src/tools/bash.ts` 的完整执行路径未逐行读完。
3. omp 的 `LICENSE` 正文（tarball 里存在 `LICENSE` 文件，本次只核对了 `package.json` 的 `"license": "MIT"` 与 `repository` 字段）。
4. 本机 `node_modules` 里的 17.3.8 与 npm 上的 18.10.4 之间是否还有其它影响块排版的改动（本次只对比了本文涉及的文件；18.10.4 引入了 `tui/src/native/*` 的 native 渲染路径，其与 `render()` 的关系未完整读完）。
5. omp 的 `!` 在**非** `useUserShell`（即模型工具）路径下是否共用 `BashExecutionComponent` 的显示（18.10.4 里模型 `bash` 工具走 `ToolCard`，与 `!` 的 `BashExecutionComponent` **不是同一个组件**——这一点已核实，但两者的视觉差异未逐像素对比）。

**待 决定**：

1. **`!` 是否要写 journal**（§6.2(3)）。建议写（审计），但这会新增一条记录类型或复用 `ToolExecutionStarted`/`Settled`，需要拍板。
2. **`!` 的 `cd` 是否持久**（§6.2(1)）。建议 v1 不持久。
3. **`safe` 档下 `!` 的行为**：建议「明确报错、不可用」（依据 ADR 0012 P5「不用审批冒充 sandbox」）。需要确认文案。
4. **规则 2 的取舍**：保留 Radian 的标记列并只对齐宽度（本文建议），还是照 omp 取消标记列、改用背景带 + header 图标。后者更像 omp 但改动大、且丢信息。

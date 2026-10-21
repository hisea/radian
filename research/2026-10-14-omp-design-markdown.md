# omp 的 Markdown 渲染 —— 调研（ADR 0035 V2 的逐元素依据）

日期：2026-10-14 · 调研者：agent · 状态：只读调研

本文是 `research/2026-10-14-omp-design.md` 的分篇 1，覆盖 **§2 Markdown**。出处写法见主篇 §0（`tui/` = `@oh-my-pi/pi-tui@18.10.4`，`pca/` = `pi-coding-agent`，`utils/` = `pi-utils`）。推断都标明「推断」。

---

## 2. Markdown

### 2.1 渲染器在哪个文件、什么解析器

- **组件**：`tui/src/components/markdown.ts`（**4268 行**），`export class Markdown implements Component`（`:1834`），构造签名 `(text, paddingX, paddingY, theme, defaultTextStyle?, codeBlockIndent = 2)`（`:1929-1935`）。
- **解析器**：**marked 的一个 fork**，在 `@oh-my-pi/pi-utils` 里（`utils/src/marked/core.ts`；`utils/src/marked.ts`、`marked/inline-*.ts` 是它的分片）。`Markdown` 里 `const markdownParser = new Marked();`（`components/markdown.ts:597`）。**不是 CommonMark 参考实现**，是 marked 的兼容实现 + 六个自定义扩展。
- **六个扩展**（`components/markdown.ts:784-793` 的 `markdownParser.use({ extensions: [...] })`）：

  | 扩展 | 定义处 | 作用 |
  |---|---|---|
  | `customHrExtension` | `:629-646` | 分隔线接受更多字符（`-*_─━═=–—`），`renderer()` 返回 `""`（真正的绘制在 `#renderHrLine`） |
  | `mathBlockExtension` | `:662-682` | `$$…$$` 独立公式 |
  | `mathEnvBlockExtension` | `:738-763` | 裸 `\begin{env}…\end{env}`（`isBareMathEnvironment` `latex-to-unicode.ts:2059`） |
  | `mathExtension` | `:648-660` | 行内 `$…$` 与 `\(…\)` |
  | `boundedAutolinkExtension` | `:767-782` | 带左右边界的裸 URL/邮箱自动链接（`VALID_AUTOLINK_LEFT_BOUNDARY = /[\s*_~(]/`，`:766`） |
  | `strikethroughExtension` | `:577-595` | `~~…~~`；并且 `:795-800` 显式关掉 marked 自带的 `del`（注释：「marked's own looser rule never runs」） |

- **数学的实现在组件之外**：`tui/src/latex-to-unicode.ts`（2203 行，行内）与 `tui/src/latex-block.ts`（1460 行，2-D 排版）。
- **解析器之外还有三个「文本预处理」**（都在 `render()` 里）：
  1. `normalizeOsc8Terminators`（`:51-53`）：把 `ESC \`（ST）结尾的 OSC 8 序列改写成 BEL 结尾，**让 marked 不把 ST 的反斜杠当 Markdown 标点**（注释 `:45-48`）。这是为了 OSC 8 超链接。
  2. `replaceTabs`（`:2608`）：Tab → `DEFAULT_TAB_WIDTH = 3` 个空格（`utils/src/tab-spacing.ts:11`）。
  3. `repairOrphanClosingFence`（`:102-146`，只在**非** streaming 时跑）：修掉「孤立的结束围栏」（模型有时多写一个 ` ``` `），把文本改短。**注意**：它会让 `render()` 的输出与输入文本长度不同，所以 omp 专门有一段代码在它删字节时作废缓存（`:2580-2600`）。

### 2.2 逐元素

**共同的骨架**：`render(width)`（`:2421`）→ `#renderContentLines(tokens, …)`（`:2897`）→ 逐 token `#renderToken(token, contentWidth, nextToken?.type)`（`:3344`）。`contentWidth = max(1, width - paddingX*2)`（`:2431`）。渲染完的每一行按 `paddingX` 加左右边距、铺到 `width`（`:2998-3010`），再上下各加 `paddingY` 行（`#renderEmptyPaddingLines` `:3211-3220`）。

**块级间距是谁加的**：**每个 token 自己看下一个 token 的类型**，如果下一个不是 `space` 就 `push("")`。这是 omp 的「一个间距尺度」的实现（对照 ADR 0035 V1）。逐处：

| token | 加空行的条件 | 出处 |
|---|---|---|
| `heading` | `nextTokenType && nextTokenType !== "space"` | `components/markdown.ts:3373-3375`、`:3387-3389` |
| `paragraph` | `nextTokenType && nextTokenType !== "list" && nextTokenType !== "space"`（**列表前不加**） | `:3404-3408` |
| `code` | `nextTokenType && nextTokenType !== "space"` | `:3443-3445` |
| `blockquote` | 同上 | `:3489-3491` |
| `hr` | 同上 | `:3498-3500` |
| `table` | 同上（在 `#renderTable` 里） | `:4183-4185` |
| `list` | **不加**（注释：「Don't add spacing after lists if a space token follows (the space token will handle it)」） | `:3449-3455` |
| `space` | 自己就渲染**一行空行** | `:3510-3512` |

**这是 omp 排版整齐的关键**：`space` token（Markdown 里的空行）本身产出一行，而它前面的块看到下一个是 `space` 就不补——所以「段落 + 空行 + 段落」得到**恰好一行**空白。

#### 标题（1–6 级）

- 前缀：`headingPrefix = "#".repeat(level) + " "`（`:3363`）。
- 样式（`:3379-3385`）：
  - **1 级**：`heading(bold(underline(text)))` —— **不带 `#` 前缀**。
  - **2 级**：`heading(bold(text))` —— **不带 `#` 前缀**。
  - **3–6 级**：`heading(bold(headingPrefix + text))` —— **带 `#` 前缀**。
- **特殊**：1 级在支持 text-sizing 的终端上（`TERMINAL.textSizing`）且 `2*plainWidth <= width` 时，用 `encodeTextSizedHeading(plainText, 2)` 把标题放大成 2 倍宽、再占一行（`:3367-3377`）；那两行**绕过** `paddingX`/背景（`:2987-2990`，注释「OSC 66 sized headings must be output raw」）。
- 颜色角色：`mdHeading`（`theme/tui-adapters.ts:287`）。加粗用 `theme.bold`（chalk），下划线用 `theme.underline`。
- **前后空行**：**后面**加 1 行（条件见上表）；**前面**不加（由上一个 token 的「后面」负责）。所以「段落 + 标题」= 段落补 1 行 + 标题行。
- 超宽：走 `wrapTextWithAnsi`（`#renderContentLines` `:2916-2931`），即**折行**（不是截断）。

**与 ADR 0035 V2 的差异**：ADR 说「`#` 号保留并弱一级…文字强调色、加粗；一级标题加下划线」。omp 的 1/2 级**不显示 `#`**，只有 3+ 级显示；且 omp 的 1 级是 `bold+underline`、2 级是 `bold`、3+ 级是 `bold`。**owner 的决定（全部保留 `#`）比 omp 更可读**，但 ADR 该注明差异（见主篇 §9.4）。

#### 段落

- `#renderInlineTokens(token.tokens)`（`:3401`），然后 `hangWrapTreeGuideLines(paragraphText, width) ?? [paragraphText]`（`:3402`）——**树形字符画有「悬挂缩进」的特殊折行**（`:479-517`，判据 `TREE_GUIDE_ANCHOR_RE` `:403`、最小内容宽 `MIN_TREE_CONTENT_WIDTH = 8` `:414`）。
- 若整个段落只有一个 display math token，走 `latexToBlock` 竖排（`:3394-3399`，`soleDisplayMath` `:1484-1502`）。
- **前后空行**：后面 1 行（列表前除外）。
- 超宽：折行。

#### 粗体 / 斜体 / 删除线

- `strong` → `theme.bold(content) + stylePrefix`（`:3658-3662`）。
- `em` → `theme.italic(content) + stylePrefix`（`:3664-3668`）。
- `del` → `theme.strikethrough(content) + stylePrefix`（`:3713-3717`），`strikethrough` 是 **chalk 的删除线**（`theme/tui-adapters.ts:296`：`chalk.strikethrough(text)`）。**无专属颜色**，只用修饰符。
- `stylePrefix` 是「重开默认样式」的前缀（`#getDefaultInlineStyleContext`），用来在嵌套样式结束后恢复外层颜色——所以每个内联 token 结尾都拼一个 `stylePrefix`，最后再 `while (stylePrefix && result.endsWith(stylePrefix)) result = result.slice(...)` 去掉悬挂的（`:3743-3746`）。

#### 行内代码

- `codespan` → 先试 `codespanSwatch(token.text, swatchGlyph)`（`:1670-1674`）：**如果行内代码的内容是一个十六进制颜色**（`#rgb`/`#rrggbb`），就画一个色块 chip（`md.colorSwatch`，unicode `■`），否则 `theme.code(token.text)`（`:3670-3674`）。
- **反引号不显示**（marked 的 `token.text` 是内容，不含围栏）。
- 颜色角色：`mdCode`。

#### 链接

- 文字：`theme.link(theme.underline(linkText))`（`:3683`）——**下划线 + `mdLink`**。
- URL：**放在括号里**追加，色 `mdLinkUrl`（`:3698-3704`：`result += \`${clickableLinkText} ${theme.linkUrl(\`(${href})\`)}${stylePrefix}\``）。
- **当链接文字与 href 相同时只显示一次**（`:3701-3704`；`mailto:` 会先剥前缀再比，`:3696-3699`）；href 为空时只显示文字（不产出空 `()`，注释指向 issue #10283）。
- **OSC 8**：`formatHyperlink(text, target)`（`:1428-1440`）在 `TERMINAL.hyperlinks` 为真时包 `\x1b]8;;target\x07 … \x1b]8;;\x07`。`resolveLink`（主题可选钩子）可以改写目标而不改可见文字（`MarkdownTheme.resolveLink` `:1356`）。
- 自动链接：`boundedAutolinkExtension` 让裸 URL/邮箱成链接，但有左右边界要求（`:765-782`）。

#### 分隔线

- `#renderHrLine(width, sourceChar)`（`:3526-3529`）：`fillChar = getHrChar(sourceChar, symbols.hrChar)`，输出 `theme.hr(fillChar.repeat(Math.min(width, 80)))`。
- **宽度上限 80**（`:3528`）。
- 字符：`getHrChar`（`:609-627`）按**源码里用的字符**保持同族（`=`→`=`、`━`→`━`、`─`/`–`/`—`→对应字符，ascii 档 `hrChar === "-"` 时全部退回 `-`）。
- 颜色角色：`mdHr`。前后各 1 行（后由自己加，前由上一个 token 加）。

#### 引用

- `#applyQuoteBorder(renderedLines, width)`（`:3535-3570`）：每行前拼 `theme.quoteBorder(symbols.quoteBorder + " ")`——**竖条 + 1 空格 = 2 格**（`:3559`）。
- 内容宽度 `max(1, width - 2)`（`:3467`、`:3540`）。
- **引用内部按同一个 `#renderToken` 递归渲染**，所以引用里可以放标题/列表/代码块（`:3469-3487`）；递归时传一个 `InlineStyleContext`，其 `applyText` 是恒等、`stylePrefix` 是引用样式（`:3466-3472`、`#getQuoteStylePrefix`）。
- 引用文字：`mdQuote`（`:3544-3552` 的 `#quoteStyle`）。
- 引用末尾的纯空行被剥掉（`:3484-3486`：`while (…text === "") renderedQuoteLines.pop()`）。
- 字形：**unicode 档 `▏`**（`symbols.ts:585`），nerd 档 `│`（`:1050`），ascii 档 `\|`（`:1355` 附近）。

#### 列表（无序 / 有序 / 嵌套）

- `#renderList(token, depth, width, styleContext)`（`:3758-3894`）：
  - `indent = "  ".repeat(depth)`（`:3761`）——**每层 2 格**。
  - 标记：`ordered ? \`${startNumber + i}. \` : "- "`（`:3847`）——**无序是 `- `**（字面量，**不走 symbols**）；有序从 `token.start ?? 1` 起算（`:3763`）。
  - `firstPrefix = indent + theme.listBullet(bullet)`（`:3848`），`continuationIndent = indent + padding(visibleWidth(bullet))`（`:3851`）——**续行按标记的实际显示宽度对齐**（注释：「`10. ` is 4 cells, not 2」）。
  - 折行用 `pushWrapped(line, firstPrefix, continuationPrefix)`（`:3791-3824`）：先量 `prefixWidth`，若 `>= width` 就截断前缀并整行折；否则 `bodyWidth = width - prefixWidth` 后折，首行拼 `firstPrefix`、续行拼 `continuationPrefix`。
  - 嵌套列表：`#renderListItem` 递归时把 `parentDepth + 1` 传下去，返回的行**带 `nested: true` 标记**（`:3911-3917`），父层直接 `push` 不做二次折行（`:3865-3872`，注释解释这是为了让续行保留正确的悬挂缩进）。
- 标记色：`mdListBullet`（`:3848` 的 `theme.listBullet`）。

#### 任务列表

**omp 不渲染任务列表。** 依据：

- marked 的词法**会**产出 `checkbox` token：`utils/src/marked/core.ts:958` 的 `tokens.unshift({ type: "checkbox", raw: item.taskRaw, checked: item.checked === true })`；类型定义在 `:26-30`（`Tokens.Checkbox { type, raw, checked }`）；词法入口是 `:938-944` 的 `/^\[([ xX])\][ \t]+/`。
- 但 `components/markdown.ts` 的**块级 switch**（`#renderToken` `:3359-3522`）与**行内 switch**（`#renderInlineTokens` `:3630-3742`）都**没有 `checkbox` 分支**（`grep -n "checkbox" components/markdown.ts` **零命中**）。落到 `default:`（`:3731-3739`）后，`"text" in token` 为假（`Checkbox` 只有 `raw`/`checked`），所以**什么都不输出**。
- `#renderListItem`（`:3897-3960`）的 `else` 分支会 `#renderInlineTokens([token])`（`:3948-3953`），结果也是空串 → 该行被跳过（`:3951-3953` 只在 `text` 非空时 push）。

**结论**：omp 的 Markdown 任务列表**把 `[ ]`/`[x]` 整个吞掉**，列表项文字正常显示但**没有复选框**。omp 的 `checkbox`/`radio` 符号（`symbols.ts:574-578`）只用于 `todo` 工具（`tools/todo.ts:371-392`）与 ask 面板（`tools/ask.ts:198-203`）。**ADR 0035 V2 的「任务列表 `☐`/`☑`」不是照搬 omp**，见主篇 §9.2 第 8 条。

#### 代码块与围栏行

- `#renderToken` 的 `code` 分支（`:3412-3447`）：
  1. **mermaid 特判**：`token.lang === "mermaid"` 且有 `theme.resolveMermaidAscii` 时，调 `resolveMermaidAscii(token.text, width)` 画 ASCII 图；每行超宽则 `truncateToWidth(..., Ellipsis.Omit)`（`:3417-3434`）。失败则落到普通代码块。
  2. **围栏行显示**：`lines.push(theme.codeBlockBorder("\`\`\`" + (token.lang || "")))`（`:3438`），正文，然后 `lines.push(theme.codeBlockBorder("\`\`\`"))`（`:3442`）。色 `mdCodeBlockBorder`。
  3. 正文由 `#renderCodeBodyLines(token, codeIndent)`（`:3043-3095`）产出：
     - `literalCode = (codeBlockIndent === 0)`（`:3044`）——为真时**行不加缩进**且标记为 `literalCode`，而 `literalCode` 的行**绕过** `paddingX`（`:2994-2997`：`if (literalCodeRow) { contentLines.push(line); continue; }`）。
     - 有高亮时用 `theme.highlightCode(tokenText, lang)`（`:3055-3062`）；streaming 且有 `createHighlightStream` 时**增量高亮已完成的行**（`:3064-3079`，`#highlightStreamingLines` `:3132-3180`）；否则逐行 `theme.codeBlock(line)`（`:3088-3090`）。
     - **流式时已闭合的围栏会被高亮**（`#codeTokenHasClosingFence` `:3097-3130`），注释说这是为了让已定稿的字节与最终渲染一致。
- **缩进**：`codeIndent = padding(codeBlockIndent)`（`:3437`），默认 2 格；**助手正文传 `codeBlockIndent = 0`**（`chat/assistant-message.ts:1339` 的 `new Markdown(text, 1, 0, …, 0)`）→ 代码**与正文同列**（都在 `paddingX=1` 之后）。
- **折行：omp 会折**。`#renderContentLines`（`:2912-2933`）对每个 token 的每一行调 `wrapTextWithAnsi(renderedRow.text, contentWidth)`，**唯一跳过折行的是 `list` token、图片行、OSC 66 行**（`:2921`）——**代码行不在豁免之列**。`literalCode` 只影响**最后组装时的左右边距**（`:2994-2997`：`if (literalCodeRow) { contentLines.push(line); continue; }`，即不加 `paddingX`），**不影响折行**。所以 omp 的代码块**超宽会折行**（折出的续行仍带 `literalCode`，`:2929`）。
- **这与 Radian 的 ADR 0028 R9 直接相反**：Radian 定的是「代码块一律不折行，超出宽度的部分截掉、行尾显示 `…`」（`blocks.rs:1257-1260`、`:1289-1292` 的 `code_line_layout`）。**omp 没有这个规则**——它在 Markdown 里折行，字符画会错位（这正是 ADR 0028 R9 要解决的问题）。**建议保留 Radian 的 R9**，并**在 ADR 0035 里注明这是与 omp 的有意差异**（ADR 0035 V2 只说了「语法高亮；围栏行弱一级显示；没有背景；代码相对正文列不再额外缩进」，**没有说折行**——**建议补一句「不折行，与 ADR 0028 R9 一致」**）。
- **代码块内的 Markdown 不处理**（由 marked 的词法保证：`code` token 的 `text` 是原文）。
- 代码块**没有背景**（`#renderToken` 不设 `bgColor`）。

#### 表格

- `#renderTable(token, availableWidth, nextTokenType, styleContext)`（`:4008-4186`）：
  1. **边界开销**：`borderOverhead = 3*numCols + 1`（`:4022`，注释推导 `"│ " + (n-1)*" │ " + " │" = 2 + (n-1)*3 + 2`）。
  2. `availableForCells = availableWidth - borderOverhead`；`< numCols` 时**回退成原始 `|` 文本**（`:4026-4034`，`wrapTextWithAnsi(token.raw, availableWidth)`）。
  3. **自然宽度** `naturalWidths[i]`：表头与该列所有单元格的**最大行宽**（`:4040-4056`）。
  4. **最小宽度** `minWordWidths[i]`：该列**最长单词**的宽度，上限 `maxUnbrokenWordWidth = 30`（`:4037`、`#getLongestWordWidth` `:3964-3974`）。
  5. 若 `sum(minWordWidths) > availableForCells`：全部降到 1，再把剩余空间**按 `(minWord - 1)` 的比例分配**，余数逐个 +1（`:4060-4088`）。
  6. 若自然总宽放得下 → 直接用（每列至少 `minColumnWidths[i]`）（`:4092-4097`）；否则按 `(natural - min)` 的比例收缩 + 余数补齐（`:4098-4125`）。
  7. **绘制**：`┌─┬─┐` 顶框、表头行（**加粗**，`:4144-4150`）、`├─┼─┤` 分隔线、每个数据行、**每两行之间再加一条分隔线**（`:4174-4176`：`if (rowIndex < token.rows.length - 1) lines.push(separatorLine)`）、`└─┴─┘` 底框（`:4182`）。
  8. 单元格：`${v} ${cells.join(\` ${v} \`)} ${v}`（`:4149`、`:4171`）——**左右各 1 格内边距**。
  9. 单元格内折行：`#wrapCellText(text, maxWidth)`（`:3986-4006`）→ `wrapTextWithAnsi`，然后**每行结尾追加 `\x1b[22m\x1b[23m\x1b[39m`**（`:4005`），注释解释：原生折行故意在行尾保持 fg/bold/italic 打开以便续行重开，而表格把单元格行夹在无样式的边框字符之间，**不终止的话样式会渗进 `│` 和下一个单元格**。
- **框线字符**：取自 `symbols.table`，而 `symbols.table` **就是 `boxSharp`**（`theme/tui-adapters.ts:177`）：unicode `┌┐└┘─│┬┴├┤┼`（`symbols.ts:441-451`），ascii 全是 `+ - |`（`:1198-1210`）。
- **CJK 宽度**：全程 `visibleWidth()`（`utils.ts:340`），它是 native `Bun.stringWidth` + **韩文兼容字母修正**（`utils.ts:275-300`，按终端身份推 1 格或 2 格）。`#terminalLineWidths`（`:3976-3984`）用它量每个单元格行。

**与 Radian 的差异**：Radian 的 `md_table`（`blocks.rs:1715-1836`）**每行之间不加分隔线**（只在表头后加一条，`:1826-1832`）；列宽是「逐个把最宽的列减 1」（`:1749-1758`），不是 omp 的按比例。见主篇 §9.2 第 10 条。

#### diff

- **Markdown 层没有 diff 特判**。`diff` 只是一个**围栏语言**，走普通代码块路径，靠**高亮**着色：`highlightCode` 的 `HighlightColors` 里 `inserted`/`deleted` 取自 `toolDiffAdded`/`toolDiffRemoved`（`theme/tui-adapters.ts:40-41`），所以 diff 语法的加/删行有红绿。
- **流式时的 diff 特例**：`#createHighlightStream`（`:3181-3210`）在没有 `createHighlightStream` 时，**只对 `diff`/`patch`/`udiff` 三个语言**做「逐行高亮」的模拟（`:3199`），注释解释：「that grammar is line-local, so per-line output matches the whole-block render」。
- **真正的 diff 渲染在别处**：`tui/src/chrome/diff.ts`（377 行）的 `renderDiff(diffText, options)`（`:172`）与 `nativeDiff`（`:110`），用于 `edit` 工具的结果卡片（`tools/edit.ts:1464` 附近的 `framedToolCard`）。它有**行内高亮**：一删一增相邻时 `renderIntraLineDiff(removed, added, theme)`（`:244-254`），把变化的字符标出来。
- 颜色：`toolDiffAdded`/`toolDiffRemoved`/`toolDiffContext`（`dark.json:54-56`）。

#### 数学

**行内**（`components/markdown.ts:1474-1476`）：

```ts
function renderMathToken(text: string): string {
	return latexToUnicode(text).replace(MATH_NEWLINES, " ");
}
```

即**单行 Unicode**（`latex-to-unicode.ts:2013` 的 `latexToUnicode`）。`$…$` 与 `\(…\)` 都走这里（`mathExtension` `:648-660`，delimiter 判定在 `utils/src/math-delimiters.ts`）。**反货币规则**：`$x$123` 是字面量不是数学（注释在 `:2504-2512` 提到 `$x$123` 的 anti-currency rule）。

**独立公式**（`:3394-3399`、`:3354-3358`、`:3945-3949`）：`latexToBlock(token.text)` → **多行 2-D 排版**（`latex-block.ts:1449-1462`）。能力（据 `latex-block.ts:1443-1448` 的文档注释）：**竖排分式、伸缩定界符、矩阵栅格、算子上限/下限、绘制的根号**；顶层源码换行与 `\\` 变成竖直行。规模：`latex-block.ts` 1460 行 + `latex-to-unicode.ts` 2203 行 = **3663 行**。

**范围**（推断，据两个文件的规模与注释）：`latex-to-unicode.ts` 是一张「LaTeX 命令 → Unicode 字符」的大映射表 + 字体命令（`MATH_FONT_COMMANDS` `:365`）+ 颜色作用域（`latexColorScope` `:1348`）；`latex-block.ts` 是一个小型 2-D 盒子排版器（`vconcat` 之类）。**未逐条试算输出质量**（主篇 §10）。

**建议**：首版不照搬（主篇 §9.2 第 11 条）。

#### 图片

- **Markdown 的 `image` token 没有 case**（`grep -n '"image"' components/markdown.ts` 只有 `:1265`，在链接遍历里）。落到 `default:`（`:3731-3739`），`Image` token 有 `text`（alt）所以**渲染 alt 文本**。
- **真正的图片不在 Markdown 里**：
  - 用户消息的附图：`chat/user-message.ts:246-262` 的缩略图（`base64ImageNode`，Kitty 协议）。
  - 助手的原生图片内容块：`chat/assistant-message.ts:964-966`（`content.type === "image"` → `#nativeImages.get(...)`）。
  - 工具结果的图片：`assistant-message.ts:967-984`（`#toolImagesByCallId`）。
  - SVG 围栏与表格图表：`#liftsFigures`（`:1353-1359`）+ `chat/svg-figure.ts`、`chat/table-chart.ts`。
- 所以 **omp 也不在 Markdown 里画图**，与 Radian 的方向一致。

#### HTML

omp 有一个**完整的归一化器**（`components/markdown.ts:188-360`）：

- `normalizeHtmlEntitiesForTerminal(raw)`（`:148-186`）：解 HTML 实体。
- `normalizeHtmlForTerminal(raw, state, codeHook?)`（`:246-360`）：按 `HTML_TAG_REGEX`（`:204`，只认 `br|p|ol|ul|li|span|text|code|hr|blockquote`）逐个处理：
  - **行内标签**（`span`/`text`）保留周围空白（空白有意义）；**块级标签**之间的纯空白被丢掉（注释 `:260-268` 解释：否则 pretty-printed `<ul>\n  <li>` 会把缩进和空行泄漏到屏幕上）。
  - `<code>` 的内容走 `codeHook`（套上 inline-code 主题）而不泄漏标签（`:270-274`）。
  - `<br>` 强制换行（`appendHtmlLineBreak` `:221-225`）。
  - `<ol>`/`<ul>`/`<li>` 维护缩进状态（`htmlListIndent` `:226-229`、`HtmlListState` `:188-192`）。
- **块级 HTML**：`#renderHtmlBlock(raw, width)`（`:3572-3599`）用 `BLOCK_HTML_REGEX`（`:208`，只认 `<hr>` 与成对的 `<blockquote>`）：
  - 单独的 `<hr>` → `#renderHrLine`（规则线）。
  - `<blockquote>…</blockquote>` → `#renderHtmlBlockquote`（`:3600-3611`）→ 加引用条。
  - 其余 → `normalizeHtmlForTerminal` 后逐行输出。
- **行内 HTML 的 `<code>` 折叠**：`collapseInlineHtml(tokens)`（`:1552-1580`）把 `InlineHtml("<code>")…InlineHtml("</code>")` 之间的 token 合成一个 `codespan`。**只在存在 `<code>` 时才跑**（`:1553-1559` 的早退）。
- **HTML 注释**：`HTML_COMMENT_REGEX = /<!--[\s\S]*?-->/g`（`:203`），被删掉。

**Radian 现状**：`markdown.rs:240-243` 把 `Event::Html`/`Event::InlineHtml` 当**普通文本**（`Piece::plain(text, …)`），所以 `<br>` 会字面显示。见主篇 §9.2 第 13 条。

### 2.3 块级间距是谁加的（小结）

**Markdown 渲染器自己**，规则是「每个块 token 渲染完，若下一个 token 不是 `space` 就补一行空白；`space` token 自己产出一行」。这与**容器**的「块间恒 1 行」（`chrome/transcript-container.ts:965`）是**两层独立**的机制：

- 容器管**块之间**（用户块 / 助手块 / 工具卡片之间）。
- Markdown 渲染器管**块内部**（标题 / 段落 / 列表 / 代码块之间）。
- 两者不叠加，因为助手块的 `Markdown` 是 `paddingY = 0`（`chat/assistant-message.ts:1339`），它的首尾不会有空行；而容器会剥掉纯空白首尾行（`trimBlankEdges`）。

这正是 ADR 0035 V1「一个间距尺度」要的两条：**块间由容器、块内由渲染器**。

### 2.4 表格列宽算法与 CJK（小结）

- 算法见上（自然宽度 → 最长词下界 → 按比例收缩 → 余数补齐 → 放不下回退原文）。
- CJK：`visibleWidth()`（native + 韩文兼容字母修正）。omp **不**把 CJK 当 2 格**假设**，而是**量**——`visibleWidth` 对每个字符取终端宽度（`unicode-width` 语义 + 修正）。
- Radian 用 `unicode-width`（ADR 0028 R9），**建议不改**（主篇 §9.2 第 14 条）。

### 2.5 omp 的 Markdown 与 Radian 的元素对照速查

| 元素 | omp | Radian（`main`） | ADR 0035 V2 | 结论 |
|---|---|---|---|---|
| 解析器 | marked fork + 6 扩展 | `pulldown-cmark` + TABLES/STRIKETHROUGH | 未提 | 保持 Radian（ADR 0028 决定 3） |
| 块级间距 | 渲染器统一管（看下一个 token） | 靠源文本 | **要渲染器管** | 照 ADR 0035 |
| 标题 | 1/2 级无 `#`；3+ 级有；1 级 bold+underline | 全部合成 `#`，bold | 全部保留 `#`，dim；文字强调色+bold；1 级下划线 | 保留 决定，注明差异，加 `md_heading_mark` |
| 粗/斜 | bold / italic | bold / italic | 有 | 一致 |
| 删除线 | chalk strikethrough | **丢失**（`Inline::Plain`） | 有 | **要补** |
| 行内代码 | `mdCode`（+ 十六进制色块 chip） | `mdCode` | 代码角色 | 一致；色块 chip **不做** |
| 链接 | 下划线 + `mdLink`，URL 括号内 `mdLinkUrl`，OSC 8 | 同（无 OSC 8） | 同 | 一致 |
| 分隔线 | `─` × min(width,80)，`mdHr` | `─` × clamp(width,1,40)，`mdHr` | 弱一级、不铺满 | 保留 40，写进 ADR |
| 引用 | `▏ `（2 格），`mdQuote` | `│ `（2 格），`mdQuote` | `│ ` | 建议改 `▏` |
| 列表 | `- ` / `1. `，2 格/层，续行按标记宽度 | `• ` / `1. `，2 格/层，续行固定 2 格 | `-` / `1.`，2 格 | 改成 `-`；续行按标记宽度 |
| 任务列表 | **吞掉** | 无 | `☐`/`☑` | Radian 自己的设计，需 owner 确认 |
| 代码块 | 围栏显示；缩进 0（助手）；高亮；**超宽折行** | 围栏剥掉；缩进 0；高亮；**超宽截断 + `…`** | 围栏显示、弱一级；缩进同正文 | 照 ADR 0035 做围栏；**折行保留 Radian 的 R9**，并在 ADR 里注明与 omp 不同 |
| 表格 | 全框线；每行之间一条；按比例收缩 | 全框线；只在表头后一条；逐个减最宽 | 全框线；表头与每行之间 | 补「每行之间」 |
| diff | 高亮着色（+ `chrome/diff.ts` 的行内高亮） | `diff_role` 着色 | 红删绿增 | 一致；行内高亮不做 |
| 数学 | 行内 Unicode + 独立 2-D（3663 行） | 无 | 待调研 | 首版都不做，留后续 |
| 图片 | 不在 Markdown 里 | 无 | 未提 | 一致 |
| HTML | 完整归一化（br/hr/blockquote/code） | 当纯文本 | 未提 | 建议补一个子集 |

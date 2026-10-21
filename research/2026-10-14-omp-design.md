# omp（oh-my-pi）的整体 TUI 设计 —— 调研（为 ADR 0035 定稿）

日期：2026-10-14 · 调研者：agent · 状态：**只读调研**，未改动任何代码，未提交

本文是 ADR 0035「TUI 的视觉语言」的调研交付物：逐条给出 omp 18.10.4 的设计系统、Markdown 渲染、transcript 各种块、输入框、状态与反馈、footer、选择器面板、窄终端退化的**事实与出处**，最后逐条对照 Radian 现状，指出 ADR 0035 / 0028 修订 1–2 / 0024 修订 1 / 0027 修订 1–4 里写错、写漏或与 omp 不同的地方。

## 目录与分篇

| 篇 | 文件 | 内容 |
|---|---|---|
| 主篇（本文） | `research/2026-10-14-omp-design.md` | §0 出处与版本、§1 设计系统（主题角色、符号表、间距与宽度常量）、§9 与 Radian 逐条对照与建议、§10 未核实与待决定 |
| 分篇 1 | `research/2026-10-14-omp-design-markdown.md` | §2 Markdown：渲染器、解析器、逐元素（标题/段落/粗斜删/行内代码/链接/分隔线/引用/列表/任务列表/代码块与围栏/表格/diff/数学/图片/HTML）的前后空行、缩进、字形、颜色角色、超宽处理；表格列宽算法与 CJK |
| 分篇 2 | `research/2026-10-14-omp-design-blocks.md` | §3 transcript 每种块；§4 输入框（editor/composer）；§5 状态与反馈（spinner、状态行、计时、通知、审批框）；§6 footer / 状态栏；§7 选择器与面板；§8 窄终端与 resize |

**与 `research/2026-10-14-omp-blocks.md` 的关系**：那一篇已经覆盖的块与容器结论（`ChatBlock` 生命周期抽象、`ToolCard` 统一块、容器管块间距的 `isPlainBlank`/`trimBlankEdges`、调用/结果配对、折叠与预览、`!` 命令）**本文不重做**，只引用；本文补的是它没有覆盖的三块：**设计系统常量表**、**Markdown 逐元素**、**输入框 / footer / 面板 / 窄终端**。凡引用那一篇的地方写作「见 blocks 篇 §x」。

## 0. 出处与版本

- **版本**：omp **18.10.4**（与 blocks 篇同一版本）。
- **来源**：npm registry 的 tarball（只读，未运行 omp；未读 `~/.omp`、`~/.pi`、`~/.codex`、`~/.radian/credentials` 或任何凭据/用户数据目录）：
  - `@oh-my-pi/pi-tui@18.10.4`：`https://registry.npmjs.org/@oh-my-pi/pi-tui/-/pi-tui-18.10.4.tgz`，shasum `89db1e65de97f0c877eac4cd72ac5757256b806e`（与 registry `dist.shasum` 一致）。
  - `@oh-my-pi/pi-coding-agent@18.10.4`：shasum `48c629464ce055e8dbca0eaef8c0f8e156e34d83`（一致）。
  - `@oh-my-pi/pi-utils@18.10.4`：shasum `a5e2561eacc7c42990e30f5f26495bc2ab9150e2`（一致）。**新增**：Markdown 的解析器与数学/表格的词法在 `pi-utils` 里（`src/marked/`、`src/math-delimiters.ts`），所以本篇把 `pi-utils` 也解包来读。
- **出处写法**：`文件:行号`，文件相对各自包的根。`tui/` = `@oh-my-pi/pi-tui`；`pca/` = `@oh-my-pi/pi-coding-agent`；`utils/` = `@oh-my-pi/pi-utils`。
- **许可**：三个包的 `package.json` 都写 `"license": "MIT"`（`pca/package.json`、`tui/package.json`、`utils/package.json`），tarball 里都有 `LICENSE`：`tui`/`pca` 是 `MIT License / Copyright (c) 2025 Mario Zechner`，`utils` 是 `MIT License / Copyright (c) 2025-2026 Can Bölük`。
- **临时目录**：源码解包在 `.scratch/omp-design/`（`pkg/`、`pcapkg/`、`utilspkg/`、三个 `.tgz`），**写完本文即删除**，不在交付物里。
- **18.10.4 的一个结构性事实**（影响怎么找代码）：聊天组件在 18.10.4 从 `pca/src/modes/components/*` **搬进了 `pi-tui`**，主题与符号表也搬进了 `pi-tui`（见 blocks 篇 §0.1）。所以**主题、符号、Markdown、块、footer 全在 `pi-tui`**，`pca` 只剩控制器与业务。本文的 `tui/` 出处占绝大多数。

---

## 1. 设计系统

### 1.1 主题角色全表

omp 的主题是**一个 JSON 文件**，schema 在 `tui/src/theme/theme-schema.json`（JSON Schema，497 行）与 `tui/src/theme/schema.ts`（TS 类型）两处，**内置 `dark` 与 `light`** 在 `tui/src/theme/dark.json`（105 行）、`tui/src/theme/light.json`（103 行）。文件结构（`dark.json:2-105`）：

| 段 | 行 | 作用 |
|---|---|---|
| `$schema` | `dark.json:2` | 指向仓库里的 schema |
| `name` | `dark.json:3` | 主题名 |
| `vars` | `dark.json:4-20` | 可复用颜色变量（`cyan`/`blue`/`green`/`red`/`yellow`/`gray`/`dimGray`/`darkGray`/`accent`/`selectedBg`/`userMsgBg`/`toolPendingBg`/`toolSuccessBg`/`toolErrorBg`/`customMsgBg`） |
| `colors` | `dark.json:21-89` | **角色表**，见下表 |
| `export` | `dark.json:90-94` | HTML 导出用背景（`pageBg`/`cardBg`/`infoBg`），TUI 不读 |
| `terminal` | `dark.json:95-104` | **宿主**自己画终端时用的调色板（`background`/`foreground`/`chrome`/`widget`/`ansi[16]`）；注释明说「omp never sets the terminal's colors」（`tui/src/theme/schema.ts:41-44`） |

**角色表**（`dark.json:21-89`，共 67 个键；`light.json:20-86`，66 个键；差一个 `link`，见下）：

| 角色 | dark 值 | schema 描述（`theme-schema.json` 的 `properties.colors.properties.*.description`） |
|---|---|---|
| `accent` | `accent`（`#febc38`） | 主强调色（logo、选中项、光标） |
| `border` / `borderAccent` / `borderMuted` | `blue` / `cyan` / `darkGray` | 普通边框 / 高亮边框 / 弱边框 |
| `success` / `error` / `warning` | `green` / `red` / `yellow` | 成功 / 错误 / 警告 |
| `muted` / `dim` | `gray` / `dimGray` | 次级文字 / 更弱文字 |
| `text` | `""`（终端默认色） | 默认文字 |
| `thinkingText` | `gray` | thinking 正文 |
| `selectedBg` | `#31363f` | 列表高亮背景 |
| `userMessageBg` / `userMessageText` | `userMsgBg` / `""` | 用户消息背景 / 文字 |
| `customMessageBg` / `customMessageText` / `customMessageLabel` | `customMsgBg` / `""` / `#b281d6` | hook/扩展消息 |
| `toolPendingBg` / `toolSuccessBg` / `toolErrorBg` | `#1d2129` / `#161a1f` / `#291d1d` | 工具卡片的三种状态背景 |
| `toolTitle` / `toolOutput` | `""` / `gray` | 工具标题 / 输出 |
| `mdHeading` / `mdLink` / `mdLinkUrl` | `#febc38` / `#0088fa` / `dimGray` | Markdown 标题 / 链接文字 / 链接 URL |
| `mdCode` / `mdCodeBlock` / `mdCodeBlockBorder` | `#e5c1ff` / `#9CDCFE` / `gray` | 行内代码 / 代码块正文 / 围栏 |
| `mdQuote` / `mdQuoteBorder` / `mdHr` / `mdListBullet` | `gray` / `darkGray` / `darkGray` / `accent` | 引用文字 / 引用竖条 / 分隔线 / 列表标记 |
| `toolDiffAdded` / `toolDiffRemoved` / `toolDiffContext` | `green` / `red` / `gray` | diff 加/删/上下文 |
| `syntax*`（9 个） | VS Code 配色 | 语法高亮 |
| `thinkingOff…thinkingXhigh`（6 个） | `darkGray`…`#e5c1ff` | thinking 级别（**脚注**：schema 里 `thinkingMax` 是**可选**，缺省回退 `thinkingXhigh`，`theme-schema.json` 的 `thinkingMax` 描述） |
| `bashMode` / `pythonMode` | `cyan` / `yellow` | 输入框在 bash / python 模式下的边框色 |
| `statusLine*`（15 个） | 见 `dark.json:74-88` | 状态栏背景、分隔符、以及每个 segment 的颜色 |
| `link`（**只有 dark 有**） | `#0088fa` | 不在 schema 的 required 里，也不在 `ThemeColor` 联合里（见下） |

**核实与勘误**：

- `theme-schema.json` 的 `colors.required` 有 **66** 项；`dark.json` 的 `colors` 有 **67** 项（多一个 `link`），`light.json` 有 **66** 项（无 `link`）。`link` 在 `tui/src/theme/schema.ts` 的 `ThemeColor` 联合（`schema.ts:55-117`，61 项）与 `ThemeBg`（`schema.ts:190-198`，7 项）里都**没有**，`grep '"link"' tui/src/theme/*.ts` 零命中 —— 即 `dark.json` 里的 `link` 是**无人读取的死键**（不影响运行：loader 只按已知角色取用，见 §1.1 末）。
- schema 的 required 66 项 = `ThemeColor`(61) + `ThemeBg`(7) − 2 个可选（`assistantMessageText`、`thinkingMax`）。`assistantMessageText` 是「助手段落正文色」，可选，缺省用终端默认前景（`tui/src/theme/schema.ts:15-17` 的 `OPTIONAL_THEME_COLOR_RECORD`，注释明说「resolve to the terminal default foreground」）。
- `colors` 的 `additionalProperties: false`（`theme-schema.json`），所以 `link` 在 schema 层面**本不该出现**——这是 omp 自己的一个小不一致，读主题文件时不必效仿。

### 1.2 符号表（`symbols.ts`：unicode / nerd / ascii 各一套）

`tui/src/theme/symbols.ts`（1522 行）：

- `SymbolPreset = "unicode" | "nerd" | "ascii"`（`symbols.ts:5`）。
- `SymbolKey` 是 **270 个键**的字符串联合（`symbols.ts:10-310`；按前缀分组：`icon.*` 55、`cmd.*` 44、`lang.*` 37、`tool.*` 24、`key.*` 18、`sep.*` 14、`boxSharp.*` 11、`status.*` 11、`tab.*` 10、`thinking.*` 7、`boxRound.*` 6、`tree.*` 5、`nav.*` 5、`format.*` 4、`md.*` 4、`chip.*` 3、`checkbox.*` 2、`boxDotted.*` 2、`context.*` 2、`progress.*` 2、`radio.*` 2、`advisor.*` 1、`skill.*` 1）。
- 三套表**各自都定义全部 270 个键**（`UNICODE_SYMBOLS` `symbols.ts:390-688`、`NERD_SYMBOLS` `symbols.ts:691-1169`、`ASCII_SYMBOLS` `symbols.ts:1172-1465`；核对：三表都是 270 项，键集合完全相同）。`SYMBOL_PRESETS` 把它们绑成一张表（`symbols.ts:1468-1472`）。
- **两套 spinner**（`SpinnerType = "status" | "activity"`，`symbols.ts:1474`）：

  | preset | `status` | `activity` |
  |---|---|---|
  | unicode | `⣾⣽⣻⢿⡿⣟⣯⣷`（8 帧 braille 粗） | `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`（10 帧 braille 细） |
  | nerd | 12 帧 Nerd Font（`󱑖…󱑕`） | 同 unicode 的 braille |
  | ascii | `\| / - \` | `- \ \| /` |

  （`symbols.ts:1476-1489`。）主题可以覆盖：`themeJson.symbols.spinnerFrames` 接受**扁平数组**（两种都用）或 `{status?, activity?}` 对象（`symbols.ts:1491-1497` 的类型、`normalizeSpinnerFramesOverride` `symbols.ts:1499-1511`），解析在 `theme/loader.ts:181-182`，取用在 `theme/theme-class.ts:801-806`（`#spinnerFramesOverrides[type] ?? SPINNER_FRAMES[preset][type]`）。

- **几个关键符号（unicode / ascii 对照）**：

  | 键 | unicode（`symbols.ts`） | ascii | 用途 |
  |---|---|---|---|
  | `nav.cursor` | `❯`（:404） | `>`（:1186） | 选择器光标 |
  | `nav.expand` / `nav.collapse` | `▸` / `▾`（:406-407） | `+` / `-` | 折叠指示 |
  | `progress.filled` / `progress.empty` | `━` / `─`（:416-417） | `=` / `-` | 进度条 |
  | `md.quoteBorder` / `md.hrChar` / `md.bullet` | `▏` / `─` / `•`（:585-587） | `\|` / `-` / `*` | Markdown 引用条 / 分隔线 / 列表点 |
  | `checkbox.checked` / `unchecked` | `☑` / `☐`（:574-575） | `[x]` / `[ ]` | 任务列表 / 多选 |
  | `radio.selected` / `unselected` | `◉` / `○`（:577-578） | `(o)` / `( )` | 单选 |
  | `format.bullet` / `format.dash` | `•` / `—`（:580-581） | `*` / `-` | 工具输出里的小点 / 破折号 |
  | `format.bracketLeft` / `Right` | `⟦` / `⟧`（:582-583） | `[` / `]` | 状态行的 badge |
  | `sep.dot` | `" · "`（:411） | `" - "` | 元信息分隔符 |
  | `status.success/error/warning/info/pending/running/aborted/done` | `✔ ✘ ⚠ ⓘ ⏳ ⟳ ⏹ •`（:392-402） | `[ok] [!!] [!] [i] [*] [~] [-] *`（:1174-1184） | 工具卡片状态图标 |
  | `key.ctrl` / `key.enter` / `key.esc` / `key.tab` | `Ctrl` / `⏎` / `⎋` / `⇥`（:659-665） | 同 | 按键提示（macOS 上 `⌃`/`⌥`/`⌘`） |

  三套 preset 的完整表在 `symbols.ts` 里；**注意 `md.quoteBorder` 在 unicode 档是 `▏`（U+258F，一个 1 格竖条），不是 `│`**；nerd 档才是 `│`（`symbols.ts:1050`）。Radian 现在的 Markdown 引用用 `│ `（`crates/radian-tui/src/blocks.rs:1551`），与 omp 的 unicode 档不同。

- **`SymbolTheme`（给组件的窄接口）**在 `tui/src/symbols.ts:15-26`：`cursor`、`inputCursor`、`boxRound`、`boxSharp`、`table`、`quoteBorder`、`hrChar`、`colorSwatch?`、`spinnerFrames`。`getSymbolTheme()`（`theme/tui-adapters.ts:139-182`）把主题拼成它：`inputCursor` 在 ascii 档是 `|`、否则 `▏`（`tui-adapters.ts:170`）；`table` **就是 `boxSharp`**（`tui-adapters.ts:177`）；`spinnerFrames` 取 `activity` 那一组（`tui-adapters.ts:180`）。

### 1.3 间距与宽度常量（左边距、缩进、最大正文宽度、分隔线宽度）

**omp 没有一张集中的「间距常量表」**。间距来自三个不同的层，各自定义、各自使用：

| 常量 | 值 | 定义处 | 谁用 |
|---|---|---|---|
| **块间空行** | 恒 **1 行** | 容器：`tui/src/chrome/transcript-container.ts:965`（全量渲染）、`:1224`（滚动回放）、`:594`（视口）都是 `if (rows.length > 0) rows.push("")` | 每个块之间；块自己首尾的纯空白行被 `trimBlankEdges` 剥掉（`transcript-container.ts:203-210`，判据 `isPlainBlank` `:164-166`）。**见 blocks 篇 §1.1** |
| **块的横向内边距 `paddingX`** | 用户块 **1**、助手正文 **1**、thinking **1**、notice **1**、工具卡片 **1**、bash/eval 帧 **1** | 每个块的构造函数实参 | `Markdown(text, paddingX, paddingY, …)`（`tui/src/components/markdown.ts:1929-1935`）、`Text(text, paddingX, paddingY)`（`tui/src/components/text.ts:48`）、`Box(paddingX, paddingY, bgFn, border)`（`tui/src/components/box.ts:67`）。出处：`chat/user-message.ts:174`（`new Markdown(text, 1, 1, …)`）、`chat/assistant-message.ts:1339-1343`（`1, 0`）、`chrome/status-notice.ts:40`（`new Text(message, 1, 0)`）、`chat/bash-execution.ts:112`（`paddingX: 1`）、`chat/eval-execution.ts:83`、`tools/default-renderer.ts:220`（`paddingX: 1, paddingY: 1`） |
| **块的纵向内边距 `paddingY`** | 用户块 **1**、其余块 **0** | 同上 | **只有带背景的块留得住**（`#renderEmptyPaddingLines` 在有 `bgColor` 时把留白行铺上背景，`components/markdown.ts:3211-3220`） |
| **代码块缩进 `codeBlockIndent`** | 默认 **2**，`Markdown` 的构造参数 | `components/markdown.ts:1935`（`codeBlockIndent: number = 2`） | 助手正文传 **0**（`assistant-message.ts:1339`，`…, 0)`），所以**助手正文的代码块相对正文列不再额外缩进**；`fence-figure.ts:68` 用默认 2 |
| **引用竖条** | `quoteBorder + " "` = **2 格** | `components/markdown.ts:3559` | 引用内容宽度按 `width - 2` 算（`:3467`、`:3540`） |
| **列表缩进** | 每层 **2 格**（`"  ".repeat(depth)`） | `components/markdown.ts:3761` | 嵌套列表；续行按**标记的实际宽度**对齐（`:3848-3851`，`10. ` 是 4 格） |
| **分隔线最大宽度** | `Math.min(width, 80)` | `components/markdown.ts:3526-3529`（`#renderHrLine`） | Markdown 的 `---`。**注意：这是 omp 的正文宽度上限，不是「正文最大宽度」** |
| **正文最大宽度** | **没有**。文字铺满 `width - 2*paddingX` | `components/markdown.ts:2431`（`contentWidth = Math.max(1, width - paddingX*2)`） | 全屏宽度（唯一的例外是 Markdown 分隔线的 80 上限） |
| **块铺满终端宽度** | 背景整行铺满 | `render/utils.ts` 的 `padToWidth`、`components/box.ts` 的 `pushRow` | 见 blocks 篇 §1.4 |
| **表格最小列宽** | **3**（`min_column`） | `components/markdown.ts:4036`（`const minColumnWidths = minWordWidths` 之前的兜底）、`blocks.rs` 侧 Radian 也用 3 | 表格收缩 |
| **表格单元格内边距** | 左右各 **1 格** | `components/markdown.ts:4149`（`` `${v} ${rowParts.join(` ${v} `)} ${v}` ``） | 表头与每行 |
| **`Text`/`Box` 的 `paddingY` 默认** | `Text` 默认 `1`，`Box` 默认 `1` | `components/text.ts:48`、`components/box.ts:67` | 显式传参覆盖 |
| **`Spacer(n)`** | 显式行数 | `components/spacer.ts:21` | 块内自己加的空白（例如 `chrome/status-notice.ts:41` 的 `new Spacer(1)`） |
| **tab 宽度** | **3** | `utils/src/tab-spacing.ts:11`（`DEFAULT_TAB_WIDTH = 3`），`tui/src/utils.ts:15` 转发 | `replaceTabs`、`wrapTextWithAnsi`、`sliceWithWidth` |

**一个全局的「紧凑」开关**：`setTuiTight(tight)`（`tui/src/utils.ts:756-758`）让 `getPaddingX(basePadding)` 把每个块的 `paddingX` **减 1**（`utils.ts:764-766`：`globalTight ? Math.max(0, basePadding - 1) : basePadding`）。用它的地方是 `Text`（`text.ts:162`）、`Markdown`（`markdown.ts:2431`）、`Box`（`box.ts:186`）、`ToolCard`（`render/tool-card.ts:158`）、`layout/row.ts`（`:125`）。配置项是 `tui.tight`（`pca/src/modes/settings.ts:575-583`，默认 false，描述「Remove the 1-character horizontal padding from the left and right of the terminal output」），由 `pca/src/modes/interactive-mode.ts:1821` 在启动时 `setTuiTight(cfgTuiTight.get(settings))`、`:3601-3602` 在设置变更时重设。

**符号/字形常量**：见 §1.2。**spinner 节拍**：`SPINNER_ADVANCE_MS = 80`（`tui/src/components/loader.ts:16`）、`RENDER_INTERVAL_MS = 1000/30`（`:14`），见 §5.1。

---

## 9. 与 Radian 现状逐条对照

对照基准：`main` 的 `af49086`（本文写作时工作区 HEAD；`git log --oneline -1` = `af49086`，`main` 同一提交）。Radian 的文件都在 `crates/radian-tui/src/`（下称 `render.rs` 等）。

**先说一条会影响整节解读的事实**：ADR 0028 与修订 2 **都还没实现**。`main` 上：
- 没有 `block_gap`/`gaps`（`grep -rn "block_gap" crates/` 零命中）——即「容器管块间距」还没有；
- `marker_prefix` 仍是旧表（`render.rs:191-210`：`ToolCall => "-> "`、`ToolResult => "<- "`、`Tip => "Tip: "`，宽度 2/3/5/0 不等）；
- `InputStyle` 只有 `Lines` 与 `Box`（`render.rs:642-649`），没有 `bar`；
- `BodyLine` 没有卡片框、没有 `Output` 分隔线、没有元信息行（`blocks.rs:262-290`；`grep '"Output"' blocks.rs` 零命中）。
- 唯一落地的修订 1 内容是 `pi/blocks2` 分支上的 WIP（`git show pi/blocks2:crates/radian-tui/src/blocks.rs` 有 `block_gap`/`gaps`/`is_call_result_pair`，`:608-654`；`marker_prefix` 已改成 `"->"`/`" <-"`/`Tip => "  "`，`:191-210`），**未合入 main**。

所以下表的「Radian 现状」= `main`；括号里注明分支上的 WIP 状态。

### 9.1 设计系统

| # | omp 的事实（出处） | Radian 现状 | 结论 |
|---|---|---|---|
| 1 | 主题是**一个 JSON**，66 个必需角色 + 7 个背景，分 `vars`/`colors`/`export`/`terminal` 四段（`tui/src/theme/dark.json:2-105`、`theme-schema.json`） | `crates/radian-tui/src/theme.rs:413` 的 `Theme` 结构，**62 个 `Style` 角色**；`theme/file.rs` 读 pi 的主题 JSON（ADR 0029 已定） | **已一致**：ADR 0029 决定 1 就是「原样读 pi 的主题 JSON」。差异只在角色名映射（ADR 0029 决定 3 的表）。**建议不改**。 |
| 2 | omp 的角色**按 UI 部件命名**（`mdHeading`、`statusLinePath`、`bashMode`、`toolPendingBg`…） | Radian 的角色按语义命名（`md_heading`、`notice`、`budget_over`、`reasoning_low`…），映射表在 `theme.rs:300-410` | **ADR 0035 V1「颜色只来自角色」已满足**。**建议**：保持 Radian 的角色名（它们更少、更语义），继续用 ADR 0029 的映射表接 pi 主题文件。**不照搬** omp 的部件级角色名（会把 `Theme` 撑到 67 个字段且与 Radian 的语义分层打架）。 |
| 3 | 符号表是 **270 键 × 3 preset**（`tui/src/theme/symbols.ts:10-1465`） | `footer.rs:22-40` 有 `Glyphs { Unicode, Nerd, Ascii }`，但**只服务 footer 的图标**（`fn glyph` `footer.rs:1395`）；转义的符号是硬编码字面量（`render.rs:191-210` 的 `"-> "`、`blocks.rs:1041` 的 `SPINNER`、`blocks.rs:1577` 的 `"─".repeat(…)`） | **这是 ADR 0035 该补的**：ADR 0035 V1 说「默认字形（unicode）」，但**没有一套 `symbols` 机制**。**建议照搬 omp 的形状但缩小范围**：建一个 `symbols.rs`，只放 Radian 真正要用的键（`status.*`、`nav.*`、`md.*`、`sep.*`、`checkbox.*`、`radio.*`、`format.*`、`progress.*`），三套 preset；`glyphs = ascii` 时全表切换。**不要**照搬 270 键（其中 `cmd.*` 44 个是斜杠命令图标、`lang.*` 37 个是文件类型 emoji，Radian 首版不需要）。理由：ADR 0035 的验收单要在 ascii 下也成立，而现在 ascii 只能靠 `Glyphs` 管 footer。 |
| 4 | spinner 是**两套**（`status` 粗 braille 8 帧 / `activity` 细 braille 10 帧），可由主题覆盖（`symbols.ts:1476-1511`） | **没有 spinner 机制**：`blocks.rs:1041` 硬编码 `SPINNER = ['⠋','⠙','⠸','⠴']`（4 帧），`blocks.rs:1050` 用 `entry.text.len() % 4` 选帧（**不是按时间**） | **ADR 0027 说「一处定义，到处一样」「每帧 250 ms」，与 omp 不同**（omp 是 80 ms，`loader.ts:16`），而且 `main` 上**还没实现**。**建议**：`◜◝◞◟` 4 帧、250 ms 是 定的，可以保留（它更慢更省电），但**必须**（a）改成按时间选帧（现在按文本长度选帧，动画会随输出长度跳），（b）集中到一处，（c）进 §1.3 的 symbols 机制以便 ascii 退回 `\| / - \`。**注意**：ADR 0027 又加了「unicode 档只用覆盖面广的字符」的约束，而 `◜◝◞◟` 在 owner 的 Operator Mono 里**没有字形**（修订 4 的事实段），所以**修订 2 的 arc spinner 与修订 4 的字体约束直接冲突**——见 §9.4 的建议。 |
| 5 | 全局 `tui.tight`：所有块的 `paddingX` 减 1（`utils.ts:756-766`、`pca/src/modes/settings.ts:575`） | **没有** | **建议不照搬**。ADR 0035 的立场是「完成度优先于可配」；omp 的 tight 是为了塞进窄终端，Radian 的 `paddingX` 已经是 1，减到 0 就没有左边距了（ADR 0035 V1「一个左边距」会破）。**理由**：Radian 的间距本来就比 omp 紧一档（正文从第 3 列起，而 omp 是 `paddingX=1` 即第 2 列起）。 |

### 9.2 Markdown（详见分篇 1 §2）

| # | omp 的事实（出处） | Radian 现状 | 结论 |
|---|---|---|---|
| 1 | 解析器是 **marked 的一个 fork**（`@oh-my-pi/pi-utils/marked`，`utils/src/marked/core.ts`），扩展 6 个：`customHr`、`mathBlock`、`mathEnvBlock`、`math`、`boundedAutolink`、`strikethrough`（`components/markdown.ts:784-793`） | `pulldown-cmark`，`Options::ENABLE_TABLES \| ENABLE_STRIKETHROUGH`（`markdown.rs:81-83`） | **ADR 0028 决定 3 已定 `pulldown-cmark`**，**建议不改**。Radian 的解析器比 omp 弱的地方是：**没有数学**、**没有 HTML 归一化**、**没有表格之外的自定义 hr 字符**。逐条见下。 |
| 2 | **块级间距由 Markdown 渲染器统一管**：每个块 token 渲染完，如果下一个 token 不是 `space` 就 `push("")`（`components/markdown.ts:3373-3375` 标题、`:3404-3408` 段落、`:3443-3445` 代码块、`:3489-3491` 引用、`:3498-3500` 分隔线、`:4183-4185` 表格） | **没有**：`md_lines`（`blocks.rs:1366-1379`）只在源码**以 `\n` 结尾**时补一行空行（`:1373-1378`），块间空行取决于源文本 | **ADR 0035 V1「Markdown 块级元素之间也恰好 1 行，由 Markdown 渲染器统一管」正是要补这条**。**建议照搬 omp 的做法**（下一个 token 不是空白就补一行），理由：它让「源文本里写不写空行」不影响输出，才是「一套间距」。**但有个 omp 的坑要避开**：omp 的 `space` token 本身也渲染一行空行（`components/markdown.ts:3510-3512`），所以「段落 + 空行 + 段落」会得到**两行**空白吗？不会——因为 `space` 之前那个 token 看到下一个是 `space` 就不补。**照搬时要把这个「看下一个 token」的判据一起照搬**，否则会双倍。 |
| 3 | 标题：**1 级**用 `heading(bold(underline(text)))`，**2 级** `heading(bold(text))`，**3–6 级** `heading(bold("#"×n + " " + text))` —— 即**只有 3 级及以下才显示 `#` 号**（`components/markdown.ts:3379-3385`）。另外 1 级在支持 text-sizing 的终端上会放大 2 倍（`:3367-3377`） | `md_blocks` 对所有级别都合成 `"#".repeat(level) + " "`（`blocks.rs:1484-1493`），样式是 `md_heading`（`theme.rs:666` = pi 的 `text` + **BOLD**，**没有下划线**） | **ADR 0035 决定 3「`#`…`######` 照原样显示、弱一级，标题文字用强调色加粗，一级标题再加下划线」与 omp 不同**：omp 的 1/2 级**不显示 `#`**，3 级以下才显示。**建议**：owner 的决定（保留全部 `#`）**更好**——它让「级别」在任何终端上都可读，不依赖颜色/加粗/放大。**但 ADR 0035 该写明这一点与 omp 的差异**，并明确「`#` 弱一级」要用一个新角色（现在 `md_heading` 只有一档，`blocks.rs:1487` 把 `#` 和标题文字都用 `MdStyle::Heading`）。**建议加一个 `md_heading_mark` 角色（dim）**。 |
| 4 | 行内：`strong` → `bold`；`em` → `italic`；`del` → `strikethrough`（**chalk 的删除线**，`theme/tui-adapters.ts:296`）；`codespan` → `mdCode`；`link` → `mdLink` + **underline**，URL 用 `mdLinkUrl` 放在**括号**里（`components/markdown.ts:3697-3704`），且 **`resolveLink` 支持 OSC 8 超链接**（`formatHyperlink` `:1428-1440`，只在 `TERMINAL.hyperlinks` 时） | `Inline::{Bold, Italic, Code, LinkText, LinkUrl}`（`markdown.rs:20-30`）。**Strikethrough 映射到 `Inline::Plain`**（`markdown.rs:142`：`Tag::Strikethrough => inline.push(Inline::Plain)`）——即**删除线解析了但不显示**。链接 URL 也放在括号里（`markdown.rs:194-199`），但 **Radian 不发 OSC 8**（ADR 0028 R2 明说） | **ADR 0035 V2 的「~~删除线~~」在 omp 里是真的删除线，而 Radian 现在丢失了它**。**建议补上**：加一个 `Inline::Strikethrough` 或直接给 `MdStyle` 加一档，用 `Modifier::CROSSED_OUT`。**OSC 8 不照搬**（ADR 0028 R2 已决定，理由是与终端能力探测绑定，而 ADR 0023 不做依赖终端应答的检测）——**ADR 0035 该明确这一点与 omp 的差异**。 |
| 5 | 分隔线：字符取自 `symbols.hrChar`（unicode `─`），宽度 `Math.min(width, 80)`，上下各 1 行（`components/markdown.ts:3526-3529`、`:3498-3500`） | `"─".repeat(width.clamp(1, 40))`（`blocks.rs:1573-1581`）——**上限是 40，不是 80**；上下空行靠源文本 | **ADR 0035 V2「弱一级的细线，宽度不超过正文宽度上限（不铺满整屏）」**：omp 的 80 是**硬上限**，Radian 的 40 更紧。**建议**：取 `min(正文宽度, 40)` 保持不变（更「不铺满整屏」），但**写进 ADR 0035 作为一个明确的常量**，别让 40 继续是 `blocks.rs` 里的魔数。 |
| 6 | 引用：`quoteBorder`（unicode `▏`）+ 空格 = **2 格**，内容宽 `width - 2`，文字用 `mdQuote`（`components/markdown.ts:3535-3568`） | `"│ "`（`blocks.rs:1551`）+ 内容宽 `width - 2`（`:1544`），文字 `mdQuote`（`theme.rs:669` = muted） | **宽度一致（2 格）**，字形不同（omp unicode 档是 `▏`）。**建议**：进 §9.1 第 3 条的 symbols 表，unicode 用 `▏`（更细，符合 ADR 0035 V2「左侧 `│`（弱一级）加 1 格」的**精神**——`▏` 比 `│` 更弱），ascii 用 `\|`。ADR 0035 V2 写的 `│` 是照截图，**建议改成 `▏`** 并注明。 |
| 7 | 列表：无序 `- `、有序 `1. `，标记色 `mdListBullet`，嵌套 `"  ".repeat(depth)`，续行按**标记实际宽度**对齐（`components/markdown.ts:3761`、`:3848-3851`） | 无序 `• `、有序 `1. `（`blocks.rs:1496-1499`），嵌套 `indent + 2`（`:1501`），标记色 `md_list_bullet`（`theme.rs:672` = pi 的 VIOLET） | **差异一**：omp 的**无序标记是 `- `**（源码字面量，不是 `•`），Radian 是 `• `。**ADR 0035 V2 写的是「无序 `-`」——与 omp 一致，与 Radian 现状不一致**。**建议按 ADR 0035 改成 `-`**，理由：`-` 在 ascii 下无需替换（omp 的 `md.bullet` 是 `•`/`*`，而列表标记它根本没用 symbols，是硬编码 `- `）。**差异二**：omp 的**续行按标记宽度对齐**，Radian 用固定 `indent + 2`（`md_wrap` 的 `indent` 参数），所以 `10. ` 的续行会**错位一格**。**建议照搬 omp 的「按标记实际宽度对齐」**。 |
| 8 | **任务列表**：omp 的 marked 词法**产出 `checkbox` token**（`utils/src/marked/core.ts:958`，`Tokens.Checkbox { type, raw, checked }` `:26-30`），但 `components/markdown.ts` 的**块级与行内 switch 都没有 `checkbox` 分支**（`grep -n "checkbox" components/markdown.ts` 零命中），所以落到 `default:` → `"text" in token` 为假 → **什么都不渲染**。即 **omp 的 Markdown 任务列表把 `[ ]`/`[x]` 整个吞掉** | **没有任务列表**：`pulldown-cmark` 的 `ENABLE_TASKLISTS` 未开（`markdown.rs:81-83` 只有 TABLES 与 STRIKETHROUGH） | **ADR 0035 V2「任务列表 `☐`/`☑`」在 omp 里并不存在**（omp 有 checkbox 符号 `symbols.ts:574-575`，但只用于 `todo` 工具与 ask 面板，见分篇 2 §3）。**建议**：ADR 0035 这一行**不是照搬 omp，而是 Radian 自己的设计**——请 owner 确认；实现上要开 `ENABLE_TASKLISTS` 并加 `MdStyle` 的 checkbox 档 + ascii 退回 `[ ]`/`[x]`。**建议照做**，因为 omp 的「吞掉」显然不是有意的。 |
| 9 | 代码块：**围栏行显示**（`` ```lang `` 与 `` ``` ``，色 `mdCodeBlockBorder`），正文高亮（`mdCodeBlock` 兜底），缩进由 `codeBlockIndent`（助手正文传 0，所以**贴正文列**）（`components/markdown.ts:3437-3445`、`:3043-3095`、`:1339`）。**超宽会折行**（`:2921-2931` 的折行豁免只有 list/图片/OSC66；`literalCode` 只影响左右边距，`:2994-2997`） | 围栏行**不显示**（`code_lines` 把首尾围栏行当 markup 剥掉，`blocks.rs:1271-1284`），代码**不折行**、超宽截断加 `…`（ADR 0028 R9，`blocks.rs:1289-1292`），缩进 0（`blocks.rs` 无 `codeBlockIndent`） | **ADR 0035 V2「围栏行（`` ```lang ``）弱一级显示，作为代码块的标题与结尾」与 omp 一致，与 Radian 现状不一致**（Radian 剥掉围栏）。**建议照搬 omp**：显示围栏行、用 `md_code_block_border` 角色。**但折行不照搬**：omp 在 Markdown 里折行，字符画会错位（正是 ADR 0028 R9 要解决的问题）——**保留 Radian 的 R9**，并在 ADR 0035 V2 的代码块一行里**补一句「不折行，与 ADR 0028 R9 一致」**（ADR 0035 现在没提折行）。**代码贴不贴左**：ADR 0035 决定 2 说「不贴第 1 列，与正文同一左边距」——**omp 恰好就是这样**（助手正文传 `codeBlockIndent=0`，代码与正文同在 `paddingX=1` 之后）。ADR 0035 背景段写的「omp 截图里代码贴左边」**与源码不符**：那是 `read` 工具的 code cell（`render/code-cell.ts:202`，走 `renderOutputBlock`，有边框+1 格内边距），不是助手正文的 Markdown。**建议在 ADR 0035 里更正这一句**。 |
| 10 | 表格：**全框线**（`┌┬┐`/`├┼┤`/`└┴┘`/`│`，取自 `symbols.table` = `boxSharp`），单元格左右各 1 格，表头加粗、表头与数据行之间有分隔线、**每两行之间也有分隔线**（`components/markdown.ts:4128-4185`）；列宽算法：先算自然宽度与「最长词」下界，再按比例收缩（`:4026-4126`）；**放不下时回退成原始 `\|` 文本**（`:4026-4034`） | `md_table`（`blocks.rs:1715-1836`）：**全框线**（`┌┬┐` 等），单元格 ` format!(" {} ", …)`（左右各 1 格），表头后一条分隔线，**数据行之间没有分隔线**（`:1826-1832` 只在 `row_index == 0` 后加一次）；列宽：自然宽度 + `min_column = 3`，**逐个把最宽的列减 1**（`:1746-1758`）；放不下时回退成原始文本（`md_table_fallback`） | **差异**：omp **每行之间都有分隔线**，Radian **只有表头下一条**。**建议照搬 omp**（每行之间一条），理由：ADR 0035 V2 明写「表头与每行之间有横线」，而 Radian 现在是「表头与数据之间」。**列宽算法**：omp 是「按比例收缩 + 补齐余数」（`components/markdown.ts:4090-4125`），Radian 是「逐个减最宽列」（`blocks.rs:1749-1758`）。两者都能收敛，**omp 的分配更均匀**；但 Radian 的算法更简单且已有测试。**建议**：先不换算法，只把「每行之间加分隔线」补上；若 owner 要更均匀的收缩，再照搬 omp。 |
| 11 | **数学**：行内 `$…$` → `latexToUnicode`（单行 Unicode，`components/markdown.ts:1474-1476`）；独立 `$$…$$`/`\[…\]`/`\begin{}…\end{}` → `latexToBlock`（**真的竖排分式、矩阵、根号、上下限**，`latex-block.ts:1449-1462`，1460 行实现 + `latex-to-unicode.ts` 2203 行） | **没有数学**。`$` 是普通文本 | **ADR 0035 V2 的「数学」一行标着「待调研」，本文给出答案**：omp 的实现规模很大（3663 行两个文件），**行内**是 `latexToUnicode`（把 `\frac{a}{b}` 之类映射成 Unicode 字符），**独立公式**是 `latexToBlock`（2-D 排版：分式叠行、矩阵栅格、伸缩定界符、根号绘制）。**建议**：**首版不照搬**。理由：（a）3600 行纯排版代码与 ADR 0028 R8 的 mermaid 决策（vendor codex-mermaid 只有 ~1 个文件）不是一个量级；（b）Radian 已经有 mermaid（结构图）与 diff（代码），数学是**第三类**「非文本排版」，首版做不完；（c）ADR 0035 的验收单只要求「覆盖 V2 全部元素的样例 Markdown」，数学可以降级为「照原文显示」。**建议 ADR 0035 改成**：首版行内数学**照原文显示**（不解析），独立公式**照原文显示**；把 `latexToUnicode` 那一路（**只**做行内、约 2200 行）留作后续切片，因为它是「用户收益/实现成本」最好的一档。**若 owner 要数学**，建议只 vendor `latex-to-unicode.ts` 的**映射表**（那是一张命令→字符的大表，可以精简成 Radian 常用子集），不要 vendor `latex-block.ts`。 |
| 12 | **图片**：Markdown 的 `image` token **没有 case**（`grep -n '"image"' components/markdown.ts` 只有 `:1265` 一处，在链接遍历里），落到 `default:` → 渲染 alt 文本（`components/markdown.ts:3731-3739`）。真正的图片走**独立的块**（`chat/user-message.ts:246-262` 的缩略图、`chat/assistant-message.ts:964-966` 的 `nativeImages`），用 Kitty/Sixel 协议 | Radian 的 `Event::Html`/`InlineHtml` 当普通文本（`markdown.rs:240-243`）；**没有图片** | **一致的方向**：omp 也不在 Markdown 里画图。**建议不照搬**（Radian 首版没有图片协议，ADR 0028 R8 的 mermaid 已经覆盖「图」的需求）。 |
| 13 | **HTML**：omp 有一个 100+ 行的归一化器 `normalizeHtmlForTerminal`（`components/markdown.ts:246-360`），把 `<br>`/`<p>`/`<ol>`/`<ul>`/`<li>`/`<code>`/`<hr>`/`<blockquote>` 变成终端文本，`<hr>` 变成规则线、`<blockquote>` 加引用条（`:3572-3611`），并且**行内的 `<code>` 会被折叠成 codespan**（`collapseInlineHtml` `:1552-1580`） | `Event::Html(text) \| Event::InlineHtml(text) => append(Piece::plain(text, …))`（`markdown.rs:240-243`）——**原样当文本显示**（包括标签本身） | **Radian 现状比 omp 差**：模型偶尔会在回复里带 HTML，Radian 会把 `<br>` 当字面量显示。**建议照搬 omp 的**「`<br>` → 换行、`<hr>` → 规则线、`<blockquote>` → 引用、其余标签去掉但保留文本」的**子集**（不需要 omp 那么全，`<ol>/<ul>/<li>` 的处理可以不做）。理由：这是**用户可见的 bug**（HTML 泄漏到屏幕），成本约 50 行。**ADR 0035 V2 完全没提 HTML**，**建议补一行**。 |
| 14 | **CJK 宽度**：omp 用 `visibleWidth()`（`utils.ts:340`，缓存 + ASCII 快路径），底层是 native 的 `Bun.stringWidth` 加**韩文兼容字母（U+3131–3163）的特殊修正**（`utils.ts:275-300`：`Hangul Compatibility Jamo` 在 macOS 终端常画 1 格，omp 从终端身份推 `setHangulCompatibilityJamoWidth`）；表格列宽用 `visibleWidth` 逐格算 | `unicode-width`（`blocks.rs:18` 的 `UnicodeWidthChar/Str`），`md_table` 的 `natural`/`pad_to` 都用它（`:1721-1740`、`blocks.rs:1931`） | **ADR 0028 R9 已定 `unicode-width`**，**建议不改**。omp 的韩文修正**不照搬**（它是为了对齐 macOS 终端的实际渲染，Radian 不做依赖终端应答的探测，ADR 0023）。**但 ADR 0035 该写明**：`unicode-width` 对韩文兼容字母按 0/1/2 判定可能与某些终端不一致——这是**已知的、接受的**偏差。 |

### 9.3 transcript 的每种块、输入框、状态、footer、面板、窄终端（详见分篇 2 §3–§8）

**逐条对照表在分篇 2 的 §8.5**（那里把 omp 的每个块与 Radian 的 `EntryKind` 并排放）。本节只列**结论性的、需要 拍板的**：

| # | 主题 | omp 的事实（出处） | Radian 现状 | 建议 |
|---|---|---|---|---|
| 1 | **工具卡片是否一张** | **一张**：`ToolCard` 有 `ToolCardVariant = "framed" \| "plain"`（`render/tool-card.ts:27`）、`ToolCardPhase` 7 态（`:24`）、`ToolCardSnapshot { header\|status, headerMeta, phase, body, sections, footer, applyBg, contentPaddingLeft/Right, borderColor }`（`:39-54`）；两个构造器 `framedToolCard`/`plainToolCard`（`:409-424`） | **两个条目**：`EntryKind::ToolCall` 与 `EntryKind::ToolResult`（`app.rs:106-107`），各自一个前缀（`render.rs:196-197`） | **ADR 0028 已决定合成一张卡片**，**未实现**（`main` 上 `grep '"Output"' blocks.rs` 零命中）。**建议照 ADR 0028 做**，且**照 omp 的两点**：（a）卡片是 `framed`/`plain` 两种拓扑，不是一个固定形状（`list`/`grep` 这类在 omp 是 `plain`，见分篇 2 §3.3）；（b）`applyBg` 与 `borderColor` 可被 renderer 覆盖（omp 的 `edit` 调用卡片用 `borderColor: "borderMuted"`，`tools/edit.ts:1147`）。 |
| 2 | **折叠预览行数** | `DEFAULT_TERMINAL_PREVIEW_LINES = 10`（`render/render-utils.ts:119`）；bash/eval 的 `PREVIEW_LINES = 20`（`chat/execution-shared.ts:26`）；`PREVIEW_LIMITS.COLLAPSED_LINES = 3`、`EXPANDED_LINES = 12`、`COLLAPSED_ITEMS = 8`（`render/render-utils.ts:99-116`）；**并且**有一个**按视口算的动态上限** `previewWindowRows() = max(6, rows - 20)`（`:337-347`） | `ToolDisplay::preview_lines` 默认 **5**（`tools.rs:753`），固定值 | **ADR 0028 R4 定的是 `tool_preview_lines` 默认 5**，**建议保留 5**（Radian 的终端通常更窄）。**但建议照搬 omp 的「动态上限」**：`min(preview_lines, max(6, terminal_rows - 20))`，理由：5 行在 30 行的终端里也可能把卡片撑到看不见，omp 的这条规则正是为此（注释明说「keeps a volatile streaming block from growing past the viewport and stranding its top」，`:330-336`）。 |
| 3 | **状态图标** | `formatStatusIcon(status, theme, spinnerFrame)`（`render/render-utils.ts:277-300`）：`success→✔`、`error→✘`、`warning→⚠`、`info→ⓘ`、`pending→⏳`、`running→spinner 帧`（有帧时）否则 `⟳`、`aborted→⏹`、`done→•`；颜色 success/error/warning/accent/muted | 状态只用 **背景色**表示（`card_band` `render.rs:272-290`：pending/success/error 三种背景），**没有图标**；`EntryKind::ToolCall` 无状态符号 | **ADR 0028 第 4 条「运行中显示 spinner，完成后换成状态符号（✔/✘，ascii 下另取）」**，**未实现**。**建议照搬 omp 的这张表**（它是通用 Unicode，ascii 档 omp 也有 `[ok]`/`[!!]` 等，见 §1.2）。**注意**：omp 的 `pending` 是 `⏳`、`running` 是 spinner——**这两个要分开**（Radian 现在只有 `tool_pending_bg`）。 |
| 4 | **输入框 `bar`** | omp 有 **8 种** composer 形状（`components/composer/registry.ts:12-21`）：`box`（圆角框 + 状态嵌在上边框）、`band`（**默认**，见下）、`claude`、`pi`、`borderless`、`rule`、`field`、`rail`。**`rail`** 就是 ADR 0035 V3 说的 `bar`：`filledSurface: true`、`sideBorders: true`、左侧 `▎` 强调色竖条、`verticalChrome: 0`（无上下 chrome）、状态栏在下方（`components/composer/rail.ts:1-49`） | `InputStyle::{Lines, Box}`（`render.rs:642-649`），`Lines` 用上下两条规则线 + `❯ `，`Box` 用圆角框 + `❯ `（`render.rs:1436-1490`） | **ADR 0035 V3 定 `bar` 为默认**，**未实现**。**建议照 omp 的 `rail` 实现**（它的字段语义与 ADR 0035 的描述逐条对上：整行背景带 = `filledSurface` + `surfaceColor`；左侧竖条 = `ACCENT_RAIL = "▎"`；无边框无规则线 = `verticalChrome: 0` + `renderTop/renderBottom` 返回 undefined）。**两点要照搬**：（a）**状态栏的挂载点是 composer 形状的一部分**（omp 的 `ComposerStyle.statusAttachment` 有 `top-border`/`top-band`/`top-rule-chip`/`none` 四值，`components/composer/types.ts:81-84`），这正是 ADR 0035 决定 1 说的「`bar` 没有上沿规则线，spinner 要有地方放」——**omp 的答案是 `rail` 用 `statusAttachment: "none"` + 状态行作为独立的一行放在输入框上方**（`prompt/editor-top-gap.ts:20-35` 的 `EditorTopGap` 管这一行的高度）；(b) `rail` 的 `bottomBar: "full"` + `bottomBarGap: true`，即**状态栏在输入框下方、且上面隔一个空行**。**ADR 0035 说 spinner 放「输入带正上方固定一行」，而 omp 的 rail 是把状态栏放在下方**——这是**与 omp 不同的一个决定**，owner 已定（要 spinner 在输入框上方），**建议保留 owner 的决定**，但 ADR 0035 该注明「omp 的 rail 是状态栏在下方，我们改成上方」的理由。 |
| 5 | **spinner 的位置** | 两种：**ANSI 路径**在输入框上方（`statusContainer` 的 `Loader`，`pca/src/modes/interactive-mode.ts:7660-7683`），**行内容**是 `<spinner> Working… · <elapsed> · <Stop>`（`components/loader.ts:44-90` 的 `describeWorkingRow`）；**band composer** 把品牌字形变成 spinner + 计时（`status-line/segments.ts:222-228`） | **没有 spinner**：`TurnState` 只在状态栏写一个词（`render.rs:1017-1019`：`idle`/`running`/`cancelled`），无动画 | **ADR 0027 + ADR 0035 决定 1** 已定（`◜` + `Working · 12s · esc to interrupt`）。**建议照 omp 的**「spinner + 词 + 计时 + 停止键」四段结构（`components/loader.ts:44-90`），但**词**用 Radian 的 `Working`（omp 的默认词就是 `"Working…"`，`pca/src/modes/interactive-mode.ts:481`；omp **没有**「Planning…」这个词，`grep` 零命中——ADR 0035 背景段提到「"Working"/"Planning…" 这类状态行」时，**Planning 在 omp 里不存在**，**建议 ADR 0035 删掉 Planning 的例子**）。 |
| 6 | **footer / 状态栏** | omp 的状态栏是**一个组件**（`status-line/component.ts`，3813 行），有 **7 个 preset**（`status-line/presets.ts:4-107`：`default`/`minimal`/`compact`/`full`/`nerd`/`ascii`/`custom`）、**26 个 segment**（`status-line/schema.ts:2-29`）、**7 种分隔符**（`separators.ts`：`powerline`/`powerline-thin`/`slash`/`pipe`/`block`/`none`/`ascii`）、以及 `statusLine.contextLine` 的四档（`off`/`percentage`/`annotated`/`embedded`，`schema.ts:42-43`）。**`default` preset 的左右两组**：左 `pi, vim, model, mode, collab, stream, path, git, pr, context_pct, cost`，右 `session_name`（`presets.ts:6-18`） | `footer.rs`（1456 行）：4 个 preset（`Deck` 默认 / `Full` / `Pi` / `Classic`，`footer.rs:45-56`）、~20 个 item（`item_placeholders` `footer.rs:598-621`）、可配 `separator`/`separator_style`/`glyphs`/`[tui.footer.items.<id>]`（`config/mod.rs:322-337`） | **ADR 0027–4 已经比 omp 更完整**（逐字段配置、starship 颜色、阈值换色）。**omp 有三点 Radian 没有**：（a）**`contextLine` 的四档**——把上下文占用画成左右两组之间的**填充线**（`component.ts:3029-3100` 的 `#buildContextGaugeFill`），`percentage`/`annotated`/`embedded` 三档递进；Radian 只有 `context`/`context_bar` 两个 item。**建议**：这是**视觉上最有价值的一条**（ADR 0035 要「polished」），可以照搬 `percentage` 与 `embedded` 两档（`annotated` 需要 compaction 阈值，Radian 有 `CompactionInfo` 但没有「自动压缩触发点」的概念，**不建议**）。（b）**`transparent` 开关**（`types.ts:53-55`）：去掉背景填充与 powerline 端点，让状态栏继承终端背景。**建议照搬**（配置项 `tui.footer.transparent`）。（c）**`sessionAccent`**（`:53`）：按 session 名 hash 出一个强调色。**建议不照搬**（Radian 的会话没有名字，且 ADR 0035 要「颜色只来自角色」——hash 出的颜色会破坏这一条）。 |
| 7 | **选择器与面板** | 统一用 `OverlayPanel`（`chrome/overlay-box.ts:218-350`）：**圆角框**、标题嵌在**上边框**（`topBorder` `:26-42`，标题加粗 + accent）、`├──┤` 分隔线（`divider` `:44-47`）、内容左右各 1 格（`row` `:55-58`）；列表用 `SelectList`（`components/select-list.ts`，两列布局：主列默认宽 32、描述列最小 10 格、`width > 40` 才分两列，`:17-19`、`:601`） | `picker.rs`（会话选择器，405 行）用 `Block::borders(ALL)`；`question.rs`（问题表单，356 行）用另一个形状；**没有共享的面板原语** | **ADR 0024 已定「带框的 Ask 面板 + 标签页 + Submit 标签」**（照 omp 的 `overlays/ask-dialog.ts`），**未实现**。**建议**：**抽一个 `panel.rs`**（圆角框 + 标题嵌上边框 + `├──┤` 分隔线 + 左右各 1 格），让 `picker`/`question`/审批框/`/login` 共用——这正是 omp 的 `OverlayPanel` 的作用，也是 ADR 0035「完成度」最容易见效的一处。**照搬 omp 的 `topBorder` 细节**：标题两侧各留 1 空格、超宽时 `truncateToWidth`（`overlay-box.ts:26-42`）。 |
| 8 | **窄终端与 resize** | omp 的退化规则（逐条见分篇 2 §8）：状态栏**先省右侧、再从左侧末尾省**（`component.ts:3066-3120`，且 `path` 是唯一「先缩再丢」的 elastic segment，`:3069-3108`）；表格在 `overhead + columns*3 > available` 时**回退成原始文本**（`components/markdown.ts:4026-4034`）；工具卡片在 `lineWidth < max(cap+2, padding*2+2)` 时**裁框**（`render/output-block.ts:196`）；对话框高度 `max(12, floor(rows*0.7))` 并固定不随内容变（`overlays/ask-dialog.ts:116-118`、`:1121`）；`SelectList` 在 `width > 40` 才分两列 | `fit_segments`（`render.rs:1115-1147`）：**从右往左丢**，丢到只剩 1 段，然后加一个 `…`（用 `truncation` 角色）；**保留最后一段**（注释明说「the last segment is never dropped」） | **ADR 0027 C2 已定「先省略右侧的条目，再从左侧末尾开始省略」**——**与 omp 一致**。Radian 的实现多了一条「最后一段永不丢」（`render.rs:1130-1134`），**比 omp 更保守**（omp 会丢到空，`component.ts:3113` 的 `while (totalWidth() > topFillWidth && right.length > 0)`）。**建议保留 Radian 的规则**，并**补 omp 的「elastic path」**：Radian 的 `cwd` item 应该**先缩短再丢**（omp 的 `path` 会重渲染成更短的 `maxLength`，`component.ts:3069-3108`），理由：`cwd` 是用户最需要看到的，直接丢很浪费。 |

### 9.4 逐条勘误（ADR 里写错/写漏/与 omp 不同的地方）

| ADR | 原文 | 问题 | 建议 |
|---|---|---|---|
| **0035 背景段** | 「晚给了 omp 的一组截图（…），并提出两条总要求」 | 无问题 | — |
| **0035 背景段** | 「`research/2026-10-14-omp-blocks.md` 读过 omp 18.10.4 的块与容器源码…但没有覆盖 Markdown 各元素与输入框的细节」 | 准确 | — |
| **0035 V2 标题行** | 「`#` 号保留并弱一级（显示层级，决定 3）；文字强调色、加粗；一级标题加下划线；上下各 1 行空白」 | **与 omp 不同**：omp 的 1/2 级**不显示 `#`**（`components/markdown.ts:3379-3382`），3 级以下才显示（`:3384`）；omp 的 1 级是 `bold+underline`、2 级是 `bold`、3+ 级是 `bold` + `#` 前缀 | **保留 owner 的决定**（更可读），但在 ADR 里**注明与 omp 的差异**；并**补一个 `md_heading_mark`（dim）角色**给 `#` 号（现在 `blocks.rs:1487` 把 `#` 与标题文字同色）。 |
| **0035 V2 列表行** | 「无序 `-`、有序 `1.`，标记用强调色；嵌套每层缩进 2 格」 | **与 omp 一致**（omp 的无序标记字面量就是 `- `，`components/markdown.ts:3848`），**与 Radian 现状不一致**（Radian 是 `• `，`blocks.rs:1498`） | 照 ADR 0035 改成 `-`。**补一句**：续行要按标记实际宽度对齐（omp `:3848-3851`；Radian 现在固定 2 格，`10. ` 会错位）。 |
| **0035 V2 任务列表行** | 「任务列表 `☐`/`☑`」 | **omp 的 Markdown 不渲染任务列表**（checkbox token 无 case，见 §9.2 第 8 条）；`☐`/`☑` 在 omp 里只用于 `todo` 工具与 ask 面板 | **注明这是 Radian 自己的设计**（不是照搬），并说明要开 `ENABLE_TASKLISTS` + 加 `MdStyle` 档 + ascii 退回 `[ ]`/`[x]`。 |
| **0035 V2 引用行** | 「左侧 `│`（弱一级）加 1 格」 | omp 的 unicode 档是 `▏`（`symbols.ts:585`），不是 `│`（那是 nerd 档，`:1050`） | **建议改成 `▏`**（更细、更「弱一级」），ascii 用 `\|`；进 symbols 表。 |
| **0035 V2 分隔线行** | 「弱一级的细线，宽度不超过正文宽度上限（不铺满整屏）」 | omp 的上限是 **80**（`components/markdown.ts:3528`），Radian 现在是 **40**（`blocks.rs:1577`） | **把 40 写进 ADR**（作为一个明确的常量），或改成 omp 的 80。**建议保留 40**（ADR 0035 要「不铺满整屏」，40 更符合）。 |
| **0035 V2 表格行** | 「全框线（边框角色），单元格左右各 1 格内边距，表头与每行之间有横线」 | 「表头与每行之间有横线」有歧义；omp 是**每两行之间都有**（`components/markdown.ts:4174-4176`），Radian 现在**只有表头下一条**（`blocks.rs:1826-1832`） | **按 omp 做**（每行之间一条），并把 ADR 的措辞改成「每行之间」。 |
| **0035 V2 数学行** | 「行内用 Unicode 上下标（`mc²`）；独立公式能排成分式的排成分式，排不了照原文显示（**待调研**：omp 的实现与范围）」 | **已调研**：omp 行内是 `latexToUnicode`（`components/markdown.ts:1474-1476`），独立是 `latexToBlock`（真 2-D 排版，`latex-block.ts:1449-1462`），合计 **3663 行** | **建议降级**：首版两者都「照原文显示」；把「行内 `latexToUnicode` 的常用子集」留作后续切片；**不要** vendor `latex-block.ts`。**写进 ADR 的理由**：成本量级与 mermaid（vendor 一个文件）差两个数量级。 |
| **0035 V2 代码块行** | 「围栏行（`` ```lang ``）弱一级显示…；没有背景；代码相对正文列不再额外缩进」 | **与 omp 一致**（围栏显示 `components/markdown.ts:3438/3442`，缩进 0 由 `codeBlockIndent=0` 实现，`assistant-message.ts:1339`）；**与 Radian 现状不一致**（Radian 剥掉围栏，`blocks.rs:1271-1284`） | 照 ADR 0035 做。 |
| **0035 V2 diff 行** | 「红删绿增（`-`/`+` 行），围栏同代码块」 | **与 omp 一致**（`toolDiffAdded`/`toolDiffRemoved`，`dark.json:54-55`）；**ADR 0028 R4 已有**（`tools.rs:312-334` 的 `diff_role`） | 无需改。**补一句**：omp 的 diff 还有**行内高亮**（`chrome/diff.ts:244-254` 的 `renderIntraLineDiff`，一删一增相邻时标出变化的字符），Radian 没有——**建议不做**（成本高、收益小）。 |
| **0035 V2 thinking 行** | 「斜体、弱一级，折叠规则不变」 | **与 omp 一致**（`chat/assistant-message.ts:1340-1343`：`color: thinkingText, italic: true`）；**ADR 0028 R3 已有** | 无需改。**但**：omp 的 thinking **折叠后是一行 `Thought for 12s`**（`thoughtLabel` `chat/assistant-message.ts:51-55`），Radian 是 `▸ thinking (N lines)`（`blocks.rs:1045-1055`）。**两者都合理**，ADR 0035 说「折叠规则不变」→ **保留 Radian 的**。 |
| **0035 V3** | 「`tui.input_style` 加第三个值 **`bar`**…整行一条背景带，左侧一条强调色竖条 `▎`，没有边框与上下规则线；多行时背景带随行数增高；带的正上方固定一行状态行放 spinner」 | **与 omp 的 `rail` 基本一致**（`components/composer/rail.ts`），**唯一不同**：omp 的 rail 把状态栏放在**下方**（`bottomBar: "full"` + `bottomBarGap: true`），而 ADR 0035 要**上方** | 保留 owner 的决定（上方），**在 ADR 里注明与 omp 的差异与理由**（上方更靠近 turn 状态、下方留给 footer）。**并补**：`bar` 的 `verticalChrome` 是 0，所以输入框的**高度记账**要跟着改（Radian 的 `input_height` 现在固定 `rows + 2`，`render.rs:777-781`；`bar` 要改成 `rows + 0` 再加上方那 1 行状态行）。 |
| **0035 决定 1** | 「`bar` 样式在输入带**正上方固定留一行状态行**…空闲时这一行空着但**仍占位**」 | **这是 omp 的做法**：`EditorTopGap`（`prompt/editor-top-gap.ts:20-35`）在 band composer 且状态行有内容时把 gap 收成 0、否则留 1 行——即 **omp 是「有内容就贴紧、没内容留空行」，不是「永远占位」** | **ADR 0035 的「永远占位」与 omp 不同**，但**更好**（它明确消除了「输入带与 transcript 的位置不因 turn 开始或结束而跳动」——omp 的 `EditorTopGap` 注释也承认它在做同样的事，只是用「有内容才收」实现）。**建议保留 ADR 0035 的做法**，注明差异。 |
| **0035 原问题 3** | 「**标题去掉 `#`**：本 ADR 照 omp 截图去掉 `#` 号…（推翻 ADR 0028 R2 的「保持现状」）。确认吗？」 | 这是**已被 决定 3 推翻的旧文本**，但**仍留在 ADR 里**（`docs/adr/0035-tui-visual-language.md:70`），与决定 3（保留 `#`）**直接矛盾** | **建议删掉或标注「已由决定 3 推翻」**。同一处问题也在「原『需要 定』」的小标题上（它说「已决定，保留原文」，但第 3 条的内容与决定 3 冲突）。 |
| **0035 原问题 2** | 「**代码块贴左**：omp 的代码块贴第 1 列；推荐**不照搬**」 | **事实错误**：omp 的助手正文代码块**不贴左**（`codeBlockIndent=0` + `paddingX=1`，与正文同列）；贴左的是 `read` 工具的 code cell（`render/code-cell.ts`，走 `renderOutputBlock`）。见 §9.2 第 9 条 | **更正这一句**。ADR 0035 的**结论（不贴左）不变**，但它不是「不照搬 omp」，而是「与 omp 一致」。 |
| **0028 修订 1 第 2 条** | 「`ToolCall` `-> `→`->`，`ToolResult` `<- `→`<-`，`Tip:` 改为 2 格前缀加内容里的标签」 | 与 `pi/blocks2` 分支的 WIP 一致（`git show pi/blocks2:crates/radian-tui/src/render.rs:191-210` 已是 `"->"`/`" <-"`/`Tip => "  "`），**但 `main` 上未实现**；且 `ToolResult` 在 WIP 里是 `" <-"`（**前导空格**，即比 `ToolCall` 多缩 1 格），**ADR 0028 第 4 条**说的「结果的标记比调用多缩进一格以示从属」——两条**同时生效**才得到 `" <-"` | **建议在 ADR 0028 里把第 2 条与第 4 条的交互写明**（第 2 条说 `<-` 两格、第 4 条说多缩一格，合起来是 `" <-"`）。 |
| **0028 修订 1 第 1 条** | 「渲染层在相邻条目之间恰好插一行空白；条目自己首尾的**无背景**纯空白行剥掉」 | **与 omp 一致**（`transcript-container.ts:965` + `trimBlankEdges:203-210` + `isPlainBlank:164-166`），但 ADR 的判据写的是「无背景」，omp 的判据是「**不含 ANSI**」（`!/\S/.test(line)`，`:165`）——**omp 的判据更宽**（任何带 ANSI 的行都不算空白，包括只带颜色不带背景的） | **建议 ADR 把判据写清楚**：Radian 应该用「空行且 `band.is_none()`」（这是 blocks 篇 §6.1 规则 1 的结论，也更贴合 Radian 的 `BodyLine::pad` 语义）。omp 的「含 ANSI」判据对 Radian 不适用（Radian 的 `Line` 是结构化 span，没有「裸 ANSI」）。 |
| **0028 修订 2 第 4 条** | 「输出分隔线：`── Output ──`（标签弱一级）」 | **与 omp 一致**（`tools/bash.ts:629`：`{ label: uiTheme.fg("toolTitle", "Output"), content: outputLines }` → `renderOutputBlock` 画 `├── Output ──┤`，`render/output-block.ts:130-138`）。**但 omp 的标签用的是 `toolTitle` 角色**（不是弱一级） | **建议**：ADR 0028 写「标签弱一级」，omp 写 `toolTitle`。**取 ADR 的**（弱一级更符合「分隔线是结构」），或改用 `tool_title`。**在 ADR 里说明与 omp 的这一处不同**。 |
| **0028 修订 2 第 6 条** | 「元信息行：弱一级，`[` `]` 包起来，`|` 分隔…shell 写退出码、耗时、超时（例 `[exit 0 \| 0.05s \| timeout 30s]`）」 | **与 omp 一致**：omp 的 bash 统计行是 `⟦Wall: 0.05s \| Timeout: 30s⟧`（`tools/bash.ts:333-372` 的 `bashStatsParts` + `:590-597` 的 `format.bracketLeft/Right` 包裹）。**omp 的括号是 `⟦⟧`**（`symbols.ts:582-583`），ascii 是 `[]` | **建议照搬 `⟦⟧`**（ADR 0028 写的是 `[` `]`）——它一眼就是「元信息」而不是「数组」。进 symbols 表。 |
| **0028 修订 2 第 3 条** | 「调用行：`shell` 写 `$ <命令>`（`$` 弱一级，命令用强调色…）；其它工具写 `<工具名> <参数摘要>`…行首是状态：运行中 spinner，完成后 `✔`/`✘`」 | **与 omp 部分一致**：omp 的 bash 卡片 head 是 `renderStatusLine` 的 `Bash: <标题>`（**不是** `$ <命令>`；`$ <命令>` 是**用户 `!` 命令**的 `BashExecutionComponent`，`chat/bash-execution.ts:120`）。omp 的**模型 bash 工具**卡片里，命令在**第一个 section**（`tools/bash.ts:616-628` 的 `sections[0]`，内容是 `formatBashCommandLines` 的 `$ <cmd>`，`tools/bash.ts:283-295`），head 是状态行 | **ADR 0028 把两种卡片混了**：`$ <命令>` 在 omp 里是**用户 `!`** 的形状；模型 `bash` 的形状是 **head（状态行）+ section 0（`$ cmd`）+ section 1（`Output`）**。**建议 ADR 0028 明确**：Radian 的模型 `shell` 卡片照 omp 的**三段**（head / 命令 section / Output section），而**用户 `!`**（ADR 0033）照 `BashExecutionComponent` 的**上下横线夹块**（blocks 篇 §5.1 已定）。 |
| **0024 修订 1 R1** | 「选项前单选 `○`、多选 `☐`…答完以后同一块里选中的项改成 `●`/`☑`」 | **与 omp 一致**：`optionMarker(theme, multi, selected)` 单选 `radio.selected`/`unselected` = `◉`/`○`，多选 `checkbox.checked`/`unchecked` = `☑`/`☐`（`tools/ask.ts:198-203`，`symbols.ts:574-578`）。**omp 的「选中」单选是 `◉`（实心圆）**，不是 `●` | **建议照搬 `◉`**（`◉` 与 `○` 是同一族，视觉上更成对）。 |
| **0024 修订 1 R1** | 「选项描述另起一行、缩进、弱一级、前缀 `↳`」 | **与 omp 一致**（`tools/ask.ts:216-219`：`` `   ${fg("dim", "↳")} ${description}` ``，缩进 3 格） | 无需改。**补**：omp 的缩进是 **3 格**（不是「缩进」这种模糊说法），ADR 该写明。 |
| **0024 修订 1 R2** | 「光标行前 `❯`」 | **与 omp 一致**（`symbols.nav.cursor = "❯"`，`symbols.ts:404`；`SelectList` 的 `DEFAULT_CURSOR_SYMBOL = ">"` 只在无 theme 时用，`components/select-list.ts:21`） | 无需改。 |
| **0024 修订 1 R2** | 「面板底边是按键提示（弱一级，用 `·` 分隔）：`⏎ select · n note · ↑/↓ move · ⇥/←/→ question · esc cancel · alt-h hide`」 | **与 omp 的结构一致**（`overlays/ask-dialog.ts:1258-1281` 的 `#footerHintText`），但**omp 的措辞不同**：单选用 `⏎ select · n note`，多选用 `space toggle · ⏎ next`，还有 `⏎ submit · ↑/↓ scroll` 等按上下文变；`esc` 与 `alt-h` 合成一段（`esc cancel · alt-h hide`）；**omp 用 `formatKeyHint` 渲染按键**（带样式），不是纯文本 | **建议照搬 omp 的「按上下文变」**（ADR 0024 的固定串在多选/Submit 标签下不准确）。**并照搬**：按键用主题角色渲染（dim 键 + muted 说明，`chrome/keybinding-hints.ts:52-56` 的 `keyHint`）。 |
| **0024 修订 1 R2** | 「标签放不下时横向滚动，当前标签总在可见范围」 | **omp 没有横向滚动的标签**：`ask-dialog.ts:206-211` 把标签名截到 `MAX_HEADER_CHIP_WIDTH = 16`（`:119`），用 `truncateToWidth` + `Ellipsis.Unicode`；标签行本身由 `TabBar` 渲染（`components/tab-bar.ts`），**没有滚动** | **ADR 0024 的「横向滚动」在 omp 里不存在**。**建议改成 omp 的做法**（截断到 16 格）——更简单，且不会让标签行跳动。 |
| **0024 修订 1 R2** | 「`recommended` 的项在标签后加 ` (Recommended)`」 | **与 omp 一致**：`RECOMMENDED_SUFFIX = " (Recommended)"`（`overlays/ask-dialog.ts:102`）。**omp 的原生路径把后缀换成右侧 badge**（`:912-919`：`...(recommended ? { value: [span("Recommended")] } : {})`） | 无需改。**补**：omp 在**原生**渲染里是 badge、在 **ANSI** 里是后缀——Radian 只有 ANSI 一条路，用后缀即可。 |
| **0027 修订 2** | 「spinner 字形：默认 `◜ ◝ ◞ ◟`…配置 `tui.spinner = "arc" \| "braille"`…速度：每帧 **250 ms**」 | **与 omp 不同**：omp 是 **80 ms**（`components/loader.ts:16` 的 `SPINNER_ADVANCE_MS`）、两套帧（`status`/`activity`）、可由主题覆盖。**且修订 4 又加了「unicode 档只用覆盖面广的字符」，而 `◜◝◞◟` 在 owner 的 Operator Mono 里没有字形**（修订 4 事实段） | **修订 2 与修订 4 冲突，必须解决**。**建议**：（a）保留 250 ms＼�且 omp 的 80 ms 偏快）；（b）`arc` 帧**换成在目标字体里确实有的字符**——按修订 4 的字体清单，可用的圆周/弧字符很有限；**建议直接用 omp 的 `activity` 那组 braille `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`**（它覆盖面极广，也是 `braille` 档的定义），把 `arc` 作为**可选**档并**在 README 里标明它需要字体支持**；（c）把帧表放进 §9.1 第 3 条的 symbols 机制。 |
| **0027 修订 3 第 1 条** | 「默认预设改为 `deck`…第 2 行：左组 `model` · `cwd` · `git_branch`（含改动计数）· `worktree`；右组 `context_bar`（含百分比）· `reasoning`」 | **与实现一致**（`footer.rs:345-364` 的 `Preset::Deck`）。**与 omp 不同**：omp 的默认 preset 是 `default`（左 `pi, vim, model, mode, collab, stream, path, git, pr, context_pct, cost`，右 `session_name`），**没有 `deck` 这种东西** | 无需改（`deck` 是 owner 照 pi-deck 定的，不是照 omp）。 |
| **0027 修订 3 第 4 条** | 「`full` 预设里较长的写法也改用同一个格式函数」 | 实现里 `Preset::Full` 的 tokens 格式是 `↑{in} ↓{out} {cache}`（`footer.rs:379-390`），`Deck` 是 `↑{in} ↓{out} R{cache}`（`:365`）——**两者不同**，所以「同一个格式函数」指的是**数字的紧凑化**（K/M），不是格式串 | 表述可以更清楚：**建议改成**「数字一律紧凑（`timing.rs:127-131` 的 K/M），`full` 的格式串保持自己的写法」。 |
| **0027 修订 4 第 2 条** | 「每个内建条目的 unicode 图标、spinner 帧与分隔符，必须在一组主流编程字体里**都有**字形…检查用一个脚本（fontTools 读 cmap），结果写进 `crates/radian-tui/README.md` 的表」 | **未实现**：`grep "Menlo\|SF Mono\|JetBrains\|cmap\|fontTools\|未查" crates/radian-tui/README.md` 零命中；`footer.rs:1395` 的 `glyph` 表还是旧的字面量 | **这是 ADR 0027 的实施项**，本文只指出它**与修订 2 的 arc spinner 冲突**（见上）。 |
| **0027 修订 4 第 1 条** | 「品牌字形：`nerd` 档 `󰰠`…`unicode` 档是**加粗、强调色的 `R`**…`ascii` 档 `rad`。不再用 `㎭`」 | **未实现**：`footer.rs:1049-1060` 的 `brand` item 仍是 `㎭`（ascii 才是 `rad`）；`startup.rs:22` 的 `RAD_MARK = "㎭"` 也还在 | 实施项。**注意**：`㎭` 也用在**启动画面的一行回退**（`startup.rs:72`），修订 4 只说了 footer 与「一行标记」，**启动画面那处要一并改**（ADR 里没写清）。 |
| **0027 修订 4 第 2 条** | 「ADR 0027 的 arc spinner（`◜◝◞◟`）同样受这条约束：不满足就退回满足的那一组」 | 这条**已经承认**了冲突并给了退路 | **建议**：与其「不满足就退回」，不如**直接定一组满足的**（见上），因为「本机没装的字体在表里标未查」意味着**首版根本无法确认 `◜◝◞◟` 是否满足**。 |
| **0027 修订 1** | 「新的默认预设 `full`（取代上午定的"默认 `pi`"）」 | 被修订 3 取代（默认是 `deck`），**但 `full` 的段落仍在 ADR 里**，且 `Preset::Full` 的文档注释还写着「ADR 0027」（`footer.rs:50-51`） | 实施项（文档一致性）。 |
| **0027 实施切片 1** | 「默认是两行的 `pi` 预设，`classic` 预设与今天逐字节相同」 | 已被修订 3 取代（默认 `deck`） | 文档一致性。 |
| **0027 C2** | 「`model` 的 `{display_name}`（例如 `Opus 5.5`）来自 provider 描述符（ADR 0026 D0），或者 models.dev 目录里的模型名」 | **与 omp 不同**：omp 的 `modelDisplayName`（`status-line/segments.ts:276`）会**去掉 `Claude ` 前缀**（注释「Display name of the active model (`Claude ` prefix dropped)」） | 无需改（Radian 的显示名来自目录，不是硬编码去前缀）。 |

---

## 10. 未核实与待决定

### 未核实

1. **`link` 死键是否真的无人读**：本文的结论来自「`ThemeColor` 联合与 `grep '"link"' tui/src/theme/*.ts` 都没有」。未逐行读 `theme/loader.ts` 的 `resolveThemeColors` 确认它是否会把未知键丢弃（读了 `loader.ts:146-200` 的骨架，`fgColors` 是按 `ThemeColor` 的已知集合填的，所以**强烈倾向**是丢弃，但**未 100% 核实**）。
2. **omp 的原生渲染路径（TSP/native）**：18.10.4 引入了 `tui/src/native/*`（`describe()` 方法、`node()`/`card()`/`row()` 等），`tui/src/status-line/component.ts:3502-3620` 与各工具的 `describeCall/describeResult` 都在这条路上。**本文的结论以 ANSI 路径（`render()`）为准**；native 路径只在必要处引用（例如 §9.3 第 7 条的面板）。两条路的**视觉一致性**（omp 自己声称「never drift apart」，`components/composer/types.ts:4-7`）**未逐像素对比**。
3. **`SelectList` 的滚动条与窄终端**：读了 `render()`（`components/select-list.ts:398-490`），确认 `ScrollView` 的 `scrollbar: "auto"` 只在溢出时占一列、`width > 40` 才分两列。**未核实**在 80 列以下的**实际观感**（没有运行 omp）。
4. **`statusLine.contextLine` 的四档**：读了 `#buildContextGaugeFill`（`status-line/component.ts:3100-3200` 附近）与 `schema.ts:42-43` 的取值表，**未逐档核实** `annotated` 的边界标记（`context.speculation`/`context.compaction`）在什么条件下出现。
5. **omp 的 `tui.tight` 与各块 `paddingX` 的实际叠加效果**：`getPaddingX` 的调用点已全部找出（`utils.ts:764` 的读者），**未核实** `setTuiTight(true)` 时 `ToolCard` 的 `ignoreTight` 组合（`render/tool-card.ts:152-158` 有 `ignoreTight ?? true` 的默认）是否会让某些卡片**完全失去**内边距。
6. **omp 的字体覆盖检查（ADR 0027）**：本文**没有**用 fontTools 核对任何字形（那需要读 `~/.config/ghostty/config` 与系统字体，属于用户环境；本文只读了 ADR 0027 的**结论**）。
7. **数学渲染的实际输出质量**：读了 `latexToBlock`/`latexToUnicode` 的入口与规模，**未逐条试算**（例如 `\frac` 的竖排是否正确、矩阵是否对齐）。本文的建议（首版不做）**不依赖**这个核实。
8. **`status-line/footer.ts` 的 `FooterComponent` 谁在用**：`grep -rn "new FooterComponent" tui/src pca/src` 零命中，`grep -rn "FooterComponent" pca/src` 也零命中。**推断**它是给非交互/其它宿主用的旧组件（交互模式用 `StatusLineComponent`，`pca/src/modes/interactive-mode.ts:2163`）。**未核实**它的实际调用方（可能在编译后的 `dist/` 里被引用，或已被弃用）。
9. **omp 的代码块在窄终端下的实际行为**：本文核实了 **omp 的 Markdown 会折行代码**（`:2921-2931`，豁免只有 list/图片/OSC66），但**未核实** `wrapTextWithAnsi` 对代码行是否有关键字内的断行保护（它折的是「词」，注释散见 `:3800-3810` 的列表前缀逻辑）。这一点对「要不要照搬 R9」不构成影响（Radian 的不折行是**有意**的选择）。

### 待 决定

1. **ADR 0035 V2 的「数学」降到什么档**（§9.2 第 11 条）：建议首版**两者都照原文显示**，把行内 `latexToUnicode` 的常用子集留作后续。若要首版就做行内，需要确认「常用子集」的范围（`latex-to-unicode.ts` 有 2203 行，全量 vendor 不现实）。
2. **ADR 0035 V2 的「任务列表」**（§9.2 第 8 条）：omp 不渲染任务列表，所以 `☐`/`☑` 是 Radian 自己的设计。**确认要做**（要开 `ENABLE_TASKLISTS`），还是跟着 omp 不做。
3. **ADR 0035 V2 的「HTML」**（§9.2 第 13 条）：建议补一行「`<br>` → 换行、`<hr>` → 规则线、`<blockquote>` → 引用、其余标签去壳留文本」。确认范围。
4. **标题的 `#` 号角色**（§9.2 第 3 条）：需要新增一个 `md_heading_mark`（dim）角色吗？还是让 `#` 与标题文字同色（现状）？**建议新增**。
5. **分隔线宽度**（§9.2 第 5 条）：40（现状）还是 80（omp）？**建议 40**。
6. **spinner 帧**（§9.4 的 0027 修订 2 行）：`arc`（`◜◝◞◟`，可能缺字形）还是 `braille`（omp 的 `activity`，覆盖广）作为**默认**？**建议 braille 默认、arc 可选**——这与 ADR 0027 的「默认 arc」相反，需要 拍板。
7. **`bar` 的状态行位置**（§9.3 第 4 条）：上方（ADR 0035 决定 1）还是下方（omp 的 rail）？**建议保留上方**，但需要在 ADR 里写明与 omp 的差异。
8. **面板原语**（§9.3 第 7 条）：是否抽一个共享的 `panel.rs`（圆角框 + 标题嵌上边框 + `├──┤`）给 picker/question/审批/login 共用？**建议做**——它是 ADR 0035「polished」投入产出最高的一处。
9. **`contextLine`**（§9.3 第 6 条）：是否照搬 omp 的「左右两组之间的上下文填充线」（至少 `percentage` 一档）？**建议做**。
10. **`tui.footer.transparent`**（§9.3 第 6 条）：是否照搬？**建议做**（成本低，窄终端/透明终端友好）。
11. **ADR 0035 / 0028 / 0027 的文档一致性**（§9.4 的多行）：是否现在就把「已被推翻但仍留在 ADR 里的旧文本」标注清楚（例如 0035 原问题 2/3、0027 修订 1 的 `full` 默认）？**建议做**，否则后续切片会照着旧文本实施。

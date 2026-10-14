# 调研：pi / omp / codex / opencode 怎样生成与显示图表

日期：2026-10-13 · 分支 `pi/research-diagrams` · 状态：**进行中**（结论文档，不改产品代码）。

结论会写回 ADR 0028 R8。每条附出处（`文件:行号`，或 URL + 访问日期 + 原文短引用）；查不到的写
「未核实」。**本文档不再有“未完成的四问”；未核实项集中在 §6 末尾。**

---

## 1. pi（`@earendil-works/pi-coding-agent` 1.1.0，本机）

### 1.1 生成：**没有任何引导**（未核实 = 系统提示里不出现 mermaid）

`grep -rn "mermaid" dist --include=*.js` 的全部命中是：`dist/core/settings-manager.js:1110,1115,1116`
（设置读写）、`dist/modes/interactive/components/settings-selector.js:318,321,669`（`/settings` 里
的一项）、`dist/modes/interactive/interactive-mode.js:67,295,1709,4063`（接线）、
`dist/modes/interactive/components/mermaid.js`（组件本身）。**系统提示与工具描述里没有 mermaid**
（`dist/core/system-prompt.js` 无命中），也就是说 pi 不在提示里教模型画图，只在显示端处理模型
自发写出的 ` ```mermaid ` 块。

### 1.2 显示：本地渲染成 **Unicode 框线字符画**，用 `grok-mermaid`

- 组件：`dist/modes/interactive/components/mermaid.js`（74 行）。
  - `:2` `import { render } from "grok-mermaid";` —— **依赖库就是 `grok-mermaid`**；
    `:1` 从 `@earendil-works/pi-tui` 取 `Marked`。
  - `:6-8` 判定一个 token 是不是 mermaid：`type === "code"` 且 `lang` 的第一个词
    （按空白切）小写为 `mermaid`。
  - `:52-74` `createMermaidMarkdownTransformer({ getMode, theme? })` 返回一个
    **MarkdownTransformer**（`mermaid.d.ts:1-10`）：把 markdown 里的 mermaid 代码块**替换成**
    字符画（包成行内代码 span），其余原样。
  - `:54-60` 模式判断：`off`、`assistant-thinking`、或（流式且模式不是 `streaming`）→
    原样返回（= 代码块显示）。
  - `:64` `const art = render(token.text);`
  - `:65-66` `if (!art || art.width > context.availableWidth) return token.raw;` —— **放不下就退回
    原始代码块**。
  - `:67-71` 非流式且有 `warnings` 时，在代码块后加一行
    `Mermaid diagram not rendered: <第一条警告> (+N more)`，用主题的 `warning` 角色。
  - `:29-45` `styleSpan` 把库给出的语义类映射到 pi 主题：
    `border`→`borderMuted`、`text`→`text`、`edge`→`accent`、`edgeLabel`→`muted`、
    `title`→`accent`+bold、`none`→原样。**颜色只来自主题**（与 ADR 0029 同向）。
  - `:9-28` `codeSpan` 把每一行包成行内代码（反引号数取该行最长反引号串 +1；空行用
    U+00A0 占位），`:73` 用 Markdown 硬换行 `"  \n"` 保证一行一行不被重排。
- 接线：`dist/modes/interactive/interactive-mode.js:295-298` 构造该 transformer；
  `:1709` `getMarkdownTransformers()` 返回 `[mermaidMarkdownTransformer, ...扩展的 transformer]`；
  它被 pi-tui 的 Markdown 组件在解析前调用：`@earendil-works/pi-tui/dist/components/markdown.d.ts`
  的 `MarkdownOptions.transform?: (markdown: string, availableWidth: number) => string`（**带可用
  宽度**），`markdown.js:1` 用 `marked` 解析。
- 设置：`docs/settings.md:113` `markdown.mermaid` = `"off" | "final" | "streaming"`，**默认
  `"streaming"`**；`dist/core/settings-manager.js:1109-1112` 的实现是「不是 off/final 就当
  streaming」；`/settings` 里可改（`settings-selector.js:318,321,669`）。

### 1.3 流式

`markdown.mermaid = streaming` 时，**流式中的 mermaid 块也被替换成字符画**（`:56-60` 的条件只挡
`off`/thinking/非 streaming），并且 `:67` 的警告行只在**非流式**时才加——即流式期间不报"没渲染"。
另外 `grok-mermaid` 的 `render` 对**半写的**块有兜底（见 1.5 的「严格文法会去掉最后一行重试一次」），
这就是 pi 能在流式期间一直显示图的原因。

### 1.4 失败与退化

- 语法错误 / 不支持的图类型 / 太大 → `render` 返回 `null` → **原样代码块**（`:65`）。
- 图比可用宽度宽 → **原样代码块**（`:65-66`）——注意 pi **不做**换方向重排。
- 非流式且有警告 → 代码块 + 一行 warning（`:67-71`）。
- `availableWidth` 来自 pi-tui 的 transform 钩子，所以宽度判断每帧按当前宽度做。

### 1.5 库 `grok-mermaid` 0.2.3（本机 `node_modules/grok-mermaid`）

- 许可 **Apache-2.0**，仓库 `github:xl0/grok-mermaid`（`package.json`）。**零依赖**
  （`dependencies` 为空）→ 纯本地、无网络、无 WASM、无子进程（`grep fetch|http|child_process|
  import(` 在 `dist/*.js` 无命中）。`dist` 452 KB。
- 支持类型（`dist/index.d.ts`）：`graph`/`flowchart`（含 `subgraph`）、`stateDiagram`、
  `classDiagram`、`erDiagram`、`sequenceDiagram`。
- API：`render(src) -> MermaidArt | null`（`:1` 文档注释）；`diagramKind(src) -> 'flowchart'|
  'state'|'class'|'er'|'sequence' | null`（`parse.d.ts`，只看头部，用来区分"永远画不了的类型"与
  "只是语法错"：`render === null && diagramKind !== null` = 语法错）；`sourceBox(src, maxWidth)` =
  把源码框起来（返回 `MermaidArt`），是调用方在 `null` 或太宽时的常用退路（`source-box.d.ts`）；
  `toAnsi`/`DEFAULT_THEME`/`AnsiTheme` 可直接出 ANSI。
- `MermaidArt`（`types.d.ts`）：`plain: string[]`（右裁剪，供显示/复制）、`styled: Span[][]`
  （同样的行，带 `Cls` 语义类的连续段）、`width`（**显示列数**，不能从 `plain` 反推）、
  `warnings: string[]`。`Cls` = `border | text | edge | edgeLabel | title | none`；**库自己不懂
  颜色**，映射给调用方的主题。
- 上限（`dist/graph.d.ts:6-11`）：`MAX_NODES = 128`、`MAX_EDGES = 512`、`MAX_GROUPS = 24`、
  `MAX_GROUP_DEPTH = 6`、`MAX_MEMBERS = 8`；超限 → 该 `parseX` 返回 `null` → `render` 返回 `null`。
- 流式兜底（`index.d.ts` 注释原文）："A flowchart keeps whatever parsed; the stricter grammars
  additionally get one retry without their final line, which is what keeps a streaming diagram on
  screen while its last statement is half-typed." `warnings` 只由 flowchart 产生，且**只是提示**
  （"advisory only, never a reason to withhold the art"）。
- 判断：**纯 Rust 可替代性未知**（见「进度与下一步」）；它是 TypeScript。

---

## 2. omp（`@oh-my-pi/*` 17.3.8，本机；pi 的分支）

### 2.1 生成：**系统提示里有一句引导**（四个对象里唯一一个）

`src/prompts/system/system-prompt.md:15-16`：

```
{{#if renderMermaid}}
- MAY emit ` ```mermaid ` blocks; terminal renders ASCII. Only genuine structure/flow, not trivia.
```

开关来自 `src/system-prompt.ts:549-550`（`renderMermaid?: boolean`）、`:612`（默认 `true`）、
`:888`（进模板数据）。设置项 `tui.renderMermaid`（`src/config/settings-schema.ts:916-924`，
`type: "boolean"`, `default: true`, 文案 "Render Mermaid fenced code blocks as ASCII diagrams"）；
`src/modes/interactive-mode.ts:805` 应用到渲染器。

### 2.2 显示：本地渲染成字符画，库是**内联 vendored** 的

- `@oh-my-pi/pi-utils/src/mermaid-ascii.ts:1-12`：`renderMermaidAscii`（直接调）与
  `renderMermaidAsciiSafe`（`try/catch`，失败返回 `null`）；`:20-32` `extractMermaidBlocks`。
- 渲染器在 `@oh-my-pi/pi-utils/src/vendor/mermaid-ascii/`：`NOTICE` 原文说它是
  "an in-house Mermaid-diagram-to-ASCII renderer adapted from beautiful-mermaid
  (https://github.com/lukilabs/beautiful-mermaid), used under the MIT License. Copyright (c)
  2026 Craft Docs"，**只移植了 ASCII 管线**（flowchart/state、sequence、class、ER、xychart），
  丢掉了 SVG 渲染器与它的 `elkjs` 依赖、浏览器入口与 SVG 主题模块；终端显示宽度改用
  `Bun.stringWidth`；`index.ts:1-11` 补充：ASCII 渲染器用自己的 grid 布局 + A* 边路由，
  **不需要外部依赖**，且是**同步**的（不像 SVG 那条需要异步布局引擎）。
- 选项（`vendor/mermaid-ascii/ascii/index.ts:36-77` `AsciiRenderOptions`）：`useAscii`（默认
  false = Unicode 框线）、`paddingX`/`paddingY`/`boxBorderPadding`、`direction`（**覆盖**源码里的
  布局方向；只作用于 flowchart/state 的 grid 管线；注释说"Useful for re-fitting a wide `LR`
  graph into a narrow viewport by laying it out top-down"）、`colorMode`
  （`none|auto|ansi16|ansi256|truecolor|html`，默认 `auto` = **探测终端 ANSI 能力**，浏览器里是
  HTML）、`theme`。类型检测在 `:80-90`：`xychart`/`sequenceDiagram`/`classDiagram`/`erDiagram`，
  其余默认走 flowchart/state。
- 宽度策略（`@oh-my-pi/pi-coding-agent/src/modes/theme/mermaid-cache.ts`，92 行）：
  - `:14-23` `maxWidth`：**超出宽度就换到垂直方向重排**——"a wide horizontal chain collapses to a
    tall vertical column (which the terminal can scroll), and a wide vertical fan-out collapses to
    a tall horizontal column"。
  - `:25-27` 一个 `Map` 缓存渲染结果**和失败**，键是「渲染选项 + 方向变体 + 源码」；
    `:29-36` 宽度选择每次调用按缓存里的候选重新决定（**resize 不需要重渲染**）。
  - `:54-60` 默认 `colorMode: "none"`（不配色），调用方显式开启主题色。
  - `interactive-mode.ts:1241` 主题变化时 `clearMermaidCache()`。
- 显示路径（`@oh-my-pi/pi-tui/src/components/markdown.ts:2419-2441`）：token 的 `lang === "mermaid"`
  且主题提供 `resolveMermaidAscii`（`:1188-1192`，签名 `(source, maxWidth?) => string | null`）→
  用**当前内容宽度**解析；返回非 null 就把每一行推入（**比宽度还宽的行用 `truncateToWidth(…,
  Ellipsis.Omit)` 裁掉**），并跳过代码块渲染；返回 `null` → 落到下面的**原始代码块**渲染
  （`:2443-2446`）。
- 流式/滚动交互（`@oh-my-pi/pi-coding-agent/src/modes/components/assistant-message.ts`）：
  `:53-77` `containsMermaidFence`（**fence 感知**，普通代码块里的 mermaid 例子不算），注释原文：
  "Mermaid defers native-scrollback settling wholesale … its ASCII rendering resolves
  asynchronously, so even a completed fence can re-layout rows that already looked settled"；
  `:192-201` `#containsMermaidSource` 每个流式 tick 重算；`:455-480`
  `getTranscriptBlockSettledRows()` 在 `#containsMermaidSource` 为真时**返回 0**（整段不"沉降"）。

### 2.3 omp 与 pi 的差异（小结）

| | pi 1.1.0 | omp 17.3.8 |
|---|---|---|
| 库 | `grok-mermaid`（外部依赖，Apache-2.0，零依赖，TS） | **内联 vendored** `beautiful-mermaid` 的 ASCII 管线（MIT，© Craft Docs；用 `Bun.stringWidth`） |
| 支持类型 | flowchart/graph(+subgraph)、state、class、er、sequence | flowchart/state、sequence、class、er、**xychart** |
| 太宽 | 退回**原始代码块** | **换垂直方向重排**（缓存里已存的另一个方向），仍太宽的行**裁剪** |
| resize | 每帧按新宽度重算（transform 收到宽度） | 缓存 + 每次调用重选方向（不重渲染） |
| 模式 | `markdown.mermaid = off|final|streaming`（默认 streaming） | `tui.renderMermaid` **布尔**（默认 true）；渲染默认**不配色**，主题色由调用方开 |
| 生成引导 | **没有** | 系统提示一句：`MAY emit ```mermaid``` blocks; terminal renders ASCII. Only genuine structure/flow, not trivia.` |
| 流式 | 流式中也渲染；警告行只在非流式加 | 渲染默认不配色；mermaid 存在时**整段 transcript 不做 native-scrollback 沉降** |

---

## 3. codex（`openai/codex`，Rust；`main` @ 2026-10-13）

来源：`git clone --filter=blob:none --sparse`（访问日期 2026-10-13）到 `/tmp/codex`，只取
`codex-rs/{tui,mermaid,core}`。

### 3.1 生成：**没有引导**（与 pi 相同，与 omp 不同）

`grep -rn -i mermaid codex-rs/core` 只命中 `config.schema.json`（设置项），**核心提示与工具描述里
没有 mermaid**。

### 3.2 显示：**自己写的 Rust 渲染器 `codex-mermaid`**（仓内 crate）

- crate：`codex-rs/mermaid/`，包名 `codex-mermaid`（`Cargo.toml`），`license.workspace = true`
  （仓库根 `LICENSE` = Apache-2.0；`codex-rs/mermaid/Cargo.toml` 里唯一依赖是 `unicode-width`）。
  `README.md` 原文："Standalone, bounded Mermaid text renderer. It uses the existing
  `unicode-width` dependency; **no Mermaid runtime or new external production dependency is
  needed**."
- `src/lib.rs:1-12` 原文："Bounded, terminal-native Mermaid diagram **prototype**. … Graph edges own
  separate lanes and endpoint positions. Crossings never join routes. Unsupported syntax and
  outputs exceeding the caller's width return errors, leaving source fallback to the caller.
  **This crate performs no I/O and does not depend on a Mermaid implementation.**"
- API：`render(source, max_width) -> Result<String, RenderError>`（`lib.rs:55`）与
  `render_spans(...) -> Result<Vec<Vec<Span>>, RenderError>`（`Span { text, role }`，
  `Role = Node | Edge | Text`，`output.rs`）；`RenderError = Unsupported | Limit | TooWide`
  （`lib.rs:28-35`，Display 文案见 `:37-45`）。**永不返回部分结果**（`lib.rs:50-53`："no partial
  result is returned"；`README.md`："Callers should retain source on any error; the library never
  returns a partial diagram."）。
- 上限（`lib.rs:20-24`）：`MAX_SOURCE = 16 KiB`、`MAX_NODES = 16`、`MAX_EDGES = 24`、
  `MAX_LABEL = 40`、`MAX_CELLS = 64 * 1024`。**比 grok-mermaid 严得多**（后者 128 节点 / 512 边）。
- 支持子集（`README.md` 表格）：`flowchart`/`graph`（TD/TB 默认、BT、LR、RL；矩形/菱形/体育场节点；
  实线 `-->`/`---`/`<-->`、虚线 `-.->`/`-.-`/`<-.->`；管道标签与中缀标签；`&` 扇出扇入）、
  `sequenceDiagram`（隐式参与者、`participant`/`actor`、别名、`->`/`->>`/`-->`/`-->>`/`-x`/`--x`、
  自消息、`Note over A[,B]`、嵌套 `loop`/`alt`/`opt`/`critical`/`break`、每个 `alt` 一个 `else`）、
  `stateDiagram-v2`/`stateDiagram`、`classDiagram`、`erDiagram`。**明确是子集不是兼容**：
  compound state、flowchart `subgraph`、其它形状、sequence 的 activation/parallel、styling、
  front matter、directive、HTML、转义、组合/零宽字符、非可加宽度的连字**都报错**（`README.md`）。
  标识符是 ASCII 字母开头的字母/数字/下划线；文本支持普通 Unicode 与 CJK。
- 画法（`README.md`）：节点按声明/首次引用顺序、按请求方向；每条边独占 lane；交叉用 `╪` 且**不**
  合流；菱形用 `◇`；体育场用圆角；水平布局为每个端点留文字槽（可能很宽，放不下就 `TooWide`）；
  class 链接用 `◁`/`△`（继承/实现）、`◆`（组合）、`◇`（聚合）；ER 基数写作 `1`/`0..1`/`1..many`/
  `0..many`；sequence 用 `▶`/`◀`，生命线交叉 `┼`，控制片段有嵌套框；state 用 `● initial`/`◎ final`。

### 3.3 TUI 里的接线与**退化**（`codex-rs/tui/src/markdown_render/mermaid.rs`）

文件头注释原文："Bounded, theme-aware Mermaid previews for **completed** Markdown fences. Source
remains owned by the transcript. Unsupported syntax, resource limits, and terminal overflow retain
the original code block **with a reason**; diagrams **never emit terminal control sequences**.
Callers only render completed fences when Mermaid rendering is enabled."

- 只渲染**已闭合**的 fence：`has_closing_fence(input, range, content_end)`（`:19-38`）——CommonMark
  在 EOF 也会发 End 事件，所以还要看真正的闭合栅栏在不在。
- `render(source, width: Option<usize>, theme)`（`:40-88`）：`width` 默认 **120**；
  `codex_mermaid::render_spans(source, width)`；出错时先给**一行 dim 提示**（按 `RenderError` 三选一，
  `:47-55`），再把**原始源码按 mermaid 语法高亮**接在后面（`:56-58`）；成功时把 `Role` 映射到
  **从 syntect 主题派生**的样式（`foreground_style_for_scopes_with_theme(theme, ["entity.name.type",
  "support.type", "variable"])`，取不到才退回 `cyan`；`Edge` 取 `["comment"]`，取不到退回 `dim`）
  （`:61-85`）——**颜色只来自主题**（与 ADR 0029 同向）。
- 设置：`config.schema.json:6173-6177` 的 `mermaid`，**boolean，默认 `true`**，说明 "Render Mermaid
  code blocks as diagrams."，属于 `:4982-4988` 的「rich content rendering」组（`lists`/`math`/
  `mermaid`/`tables`，默认都 `true`，说明 "Rich content rendering. Independent of animations and
  visual effects."）。

### 3.4 流式（`codex-rs/tui/src/streaming/controller.rs:22-26`）

原文："Transformable fences stay mutable while their containing top-level block is last. …
**For Mermaid, the closing fence replaces source with a diagram, and resizing can replace a diagram
that no longer fits with its source.** Once another block starts, the diagram enters scrollback so
later prose does not grow the tail."

即：**流式期间不画半成品图**，闭合栅栏到达时才把源码换成图；终端变窄可以把图换回源码；
下一个块开始后图进入 scrollback（不再重排）。测试 `streaming/mermaid_tests.rs`
（`mermaid_stream_closes_resizes_and_preserves_raw_source`）逐块断言
`render.lines == render::render_source(&source, Some(80), …)` —— 渲染是
**源码 + 宽度 + 模式的纯函数**（与 ADR 0023「绘制不做 I/O」同向）。

### 3.5 另有一条**浏览器**路径（与 mermaid 无关）

`codex-rs/tui/src/inline_visualization.rs:1` 原文 "Terminal fallback for assistant-authored inline
visualization directives"：模型写 `::codex-inline-vis{…}` 指令，`viewer::materialize_document` 把
`assets/inline_visualization/visualize.html` + `visualize.css` 落成文档再看（细节**未核实**）。
另外 `codex-rs/tui/src/terminal_probe.rs:1-13` 说明它**会**在启动/恢复时做「短、尽力而为」的终端
应答探测（自己给 deadline，取不到就 `None` 退回保守默认）——即 codex 存在「问终端」的做法，
但 **mermaid 渲染本身不依赖它**（只用文本 + 主题）。

---

## 4. opencode（`anomalyco/opencode`（原 `sst/opencode`，`sst/opencode` 现在 301 到它），MIT，`dev` 分支 @ 2026-10-13）

来源：`git clone --filter=blob:none --sparse`（访问日期 2026-10-13）到 `/tmp/oc`（只取
`packages/{merman,tui,session-ui}`）+ 对 `packages/core` 里几个 prompt 文件的 raw 抓取。

### 4.1 生成：base prompt 里**没有**引导；但有一个可选插件技能教模型画图

`packages/core/src/plugin/system-prompt/{anthropic,gpt,gpt-astra,kimi,meta,trinity}.txt` +
`packages/core/src/session/runner/prompt/system.txt` + `packages/core/src/session/prompt.ts`
（8910 B）+ `packages/core/src/session/system-prompt.ts`，`grep -i "mermaid|diagram|flowchart|ascii"`
**无命中** ✓（与 pi / codex 一致）。

但仓里有一个**插件技能** `packages/plugins/figma/skills/figma-generate-diagram/SKILL.md`，
frontmatter 原文："MANDATORY prerequisite — load this skill BEFORE every `generate_diagram` tool
call. … Trigger whenever the user asks to create, generate, draw, render, sketch, or build a
diagram — flowchart, architecture diagram, sequence diagram, ERD or entity-relationship diagram,
state diagram or state machine, gantt chart, or timeline. Also trigger when the user mentions
Mermaid syntax …"；正文："`generate_diagram` takes Mermaid.js syntax and produces an editable
FigJam diagram."；同目录 `references/` 有 7 份分类型指引（architecture/erd/flowchart/gantt/
sequence/state/workflow）✓。→ 这是**插件**层面的生成引导，而且渲染目标是 **FigJam（画布/浏览器）**，
不是终端 ✓。

### 4.2 显示：**两条完全不同的路径**

#### (a) TUI（终端）：自家包 `@opencode/merman`

- `packages/merman/package.json`：name `@opencode/merman`，version **2.0.28**，license 字段缺省
  （仓库 `LICENSE` = MIT ✓）；依赖 `@opencode/plugin`、**`@opentui/core`**、`entities@7.0.1`、
  `string-width` ✓。
- 挂载：`packages/tui/src/plugin/builtins.ts:13,25` `import Merman from "@opencode/merman/plugin"`
  且把它放进 `builtins` 列表（与 `Latex` 并列）✓。
- 注册方式（`packages/merman/src/plugin.ts`，15 行）：
  `Plugin.define({ id: "opencode.merman", setup(context) { context.markdown.registerCodeBlockRenderer("mermaid", createMermaidCodeBlockRenderer(context.renderer, () => ({ colors: resolveOpenCodeDiagramPalette(context.theme, context.themeMode) }))) } })`
  —— **按语言注册代码块渲染器** ✓，颜色从**主题 + themeMode** 派生 ✓。
- 支持的家族（`packages/merman/src/`）：`flowchart`、`gantt`、`gitgraph`、`sequence`、`state`、
  `timeline` ✓（**没有 class / er** ✓）。每个家族都是 `parser.ts` / `drawing.ts` /
  `render-grid.ts` / `style.ts` 四件套（解析 → 网格绘制 → 样式 → 输出 StyledText）✓。
- 纯本地：用 `@opentui/core` 的 `TextRenderable` / `StyledText` / `parseColor` 直接画进 TUI ✓，
  无浏览器、无 WASM、无网络（是否起子进程**未核实**）。
- 宽度：`markdown.ts:52-56` `layoutMaxWidth` 默认 **120**，`:281` 实际用
  `Math.min(configuredMaxWidth, ctx.width)` ✓；选项注释原文："**Fold responsive horizontal diagrams
  that exceed this width.** Defaults to 120 columns." ✓。
- 主题（`palette.ts:1-25`）：`createOpenCodeDiagramPalette` 从 `text`/`subdued`/`info`/`success`/
  `warning`/`background`/`accent.{soft,clear}` 这些**主题 token** 派生（用 `blendColor` 算
  secondary/muted）✓ —— **颜色只来自主题**（与 ADR 0029 同向）。

#### (b) Web / session-ui（浏览器）：真的 mermaid.js → SVG

`packages/session-ui/src/components/markdown-mermaid.ts`：`await import("mermaid")`（**懒加载**
真正的 mermaid npm 包 ✓），`mermaid.initialize({ startOnLoad: false, securityLevel: "strict",
 theme: 按 document 亮/暗, flowchart: { htmlLabels: false } })`，先
`mermaid.parse(source, { suppressErrors: true })`（失败返回 `undefined` = 回退源码 ✓），成功返回
**SVG**（`mermaid.render(...).svg`）✓。→ **浏览器里是 SVG，终端里是字符画**。

### 4.3 流式：**四个对象里最激进的**（流式就画，并「粘住」上一次成功的图）

`packages/merman/src/markdown.ts:269-320`：渲染器闭包里有一个
`lastGood: Map<blockId, PreparedDiagram>`，键是 `context.defaultRender()?.id`（注释原文：
"OpenTUI's default block ID is the stable identity available for this fence across streaming
updates" ✓）。逻辑：

1. `MermaidSyntaxError` 时，**先看这个块上一次成功的图**，kind 相同就直接继续显示它 ✓（粘住旧图）；
2. 否则若错误行 ≤ 2，或错误行之后全是空行 → 返回 `undefined`（回退到普通代码块）✓；
3. 否则用**截断到错误行之前**的源码重试（`lines.slice(0, error.lineNumber - 1)`）✓，仍失败 →
   `undefined` ✓。
4. `DiagramCanvasSizeError` → `undefined` ✓；其它错误 rethrow ✓。

### 4.4 失败与退化汇总

TUI：`undefined` → 交回普通代码块；web：`undefined` → 交回源码；错误分
`MermaidSyntaxError`（带 `lineNumber`，所以能做「截到错误行前重试」）与 `DiagramCanvasSizeError`
（画布尺寸超限）✓。

---

## 5. 现成库（crates.io / npm；访问日期 **2026-10-13**）

查询方式：`crates.io` API（`/api/v1/crates/<name>` 与 `?q=` 搜索）、npm 已装包的 `package.json`、
GitHub raw / trees API。**星数、审计、issue 情况未核实**。

### 5.1 已确认可用的（能把 mermaid 变成终端文本）

| 库 | 版本 | 许可 | MSRV | 下载/最近更新 | 说明 |
|---|---|---|---|---|---|
| **`mermaid-text`** | 0.57.4 | **MIT** | **1.92**（0.11.x 声明 **1.85**） | 74,685 / 2026-07-21 | “Render Mermaid diagrams as Unicode box-drawing text — **no browser, no image protocols, pure Rust**”。18 个家族（flowchart/graph、sequence、state、class、er、gantt、gitGraph、journey、mindmap、pie、quadrant、requirement、sankey、xychart、block、packet、timeline、architecture）；`render(src)` / `render_with_width(src, Some(80))` / `render_ascii`；输出是**纯 `String`、无 ANSI**；依赖只 `unicode-width`+`ascii-dag`+`chrono`；`#![forbid(unsafe_code)]`。限制（README）：点线交叉会变实线、RL/BT 子图不反转、深层嵌套 direction 覆盖、**窄列里长标签会重叠** |
| **`merman` + `merman-ascii`** | 0.9.0 | MIT OR Apache-2.0 | **1.95** | 130,625 / 26,829 · 2026-10-12 | “Parity-focused, **headless** Rust implementation of Mermaid for parsing, layout, and rendering” + “**Terminal-friendly ASCII and Unicode renderer for Mermaid typed models**”。最贴近需求（解析与渲染分层，typed model → 文本），但 MSRV 1.95 且是否给出 span/颜色 **未核实** |
| **`figurehead`** | 0.4.3 | MIT | 未声明 | 751 / 2025-12-30 | “A Rust library to convert **Mermaid.js diagram markup into ASCII diagrams**”。下载量小、半年未更新 |
| **`selkie-rs`** | 0.3.0 | MIT | 未声明 | 10,808 / 2026-02-07 | “A Rust implementation of the mermaid diagramming parser and renderer” |
| **`ascii-dag`** | 0.11.0 | MIT OR Apache-2.0 | 未核实 | 107,885 / 2026-09-06 | “**Zero-dependency** graph layout engine that renders to text: DAGs and cyclic graphs, clusters, four flow directions, **colors**”。**不认 mermaid 源码**，只画它自己的图模型（mermaid-text 就用了它）；适合作为“自己写渲染器”的布局底座 |
| **`orthodag`** | 0.1.1 | MIT OR Apache-2.0 | **1.96** | 109 / 2026-09-24 | “Draw a directed graph as Unicode box-drawing text”（极新、极小） |

### 5.2 不能直接用（记录以免重复调研）

- **`flowmaid`** 0.27.0：GPL-3.0-or-later ⚠️（且是 “Mermaid-**like**” 自有引擎）→ 许可上不适合 Radian。
- **`mermaid-rs-renderer`** 0.3.1（171,942 下载）：23 种图、features `cli`/`png` → 目标是 **SVG/PNG 图像**，不是终端文本。
- **`mermaid-svg`** / `mermaid-little` / `simple-mermaid` / `mdbook-mermaid` / `merman-rustdoc` / `ast-to-mermaid` / `mermaid-builder`：SVG、rustdoc/mdbook 集成或**生成** mermaid，不是终端渲染。
- **`graphviz-rust`** 0.9.9（232 万下载）：crates.io 上 license 字段写 **non-standard**（需核对 ⚠️ 未核实）；`layout-rs` 0.1.3（2025-04 后未更新）是 graphviz 的 Rust 移植，都需 **DOT** 而不是 mermaid。
- **`termgraph`**（2023 停更）/ **`textplots`** / **`asciigraph`** / **`rasciigraph`**：**数据图**（柱/线），不是结构图（架构/流程/时序）。
- **`mermaid-fixer`** 1.0.2（MIT）：自动修 Markdown 里的 mermaid 语法错 —— 对**生成侧**有用（见 §6 选项 D 的可选部分）。
- **`termaid`**（Python）：`mermaid-text` 的 README 把它列为 prior art（“direction-bit canvas, barycenter heuristic constants, subgraph border padding” 都借鉴自它）→ 不可嵌入 Rust，但值得读。

### 5.3 四个对象用的是什么（汇总）

| | 渲染方式 | 库 | 语言/许可 | 网络/子进程 |
|---|---|---|---|---|
| pi | 终端字符画 | `grok-mermaid` 0.2.3 | TS / Apache-2.0 | 无 |
| omp | 终端字符画 | vendored `beautiful-mermaid` 的 ASCII 管线 | TS / MIT（© Craft Docs） | 无（宽度用 `Bun.stringWidth`） |
| codex | 终端字符画 | 自家 `codex-mermaid`（仓内） | **Rust** / Apache-2.0 | 无（"performs no I/O"） |
| opencode | 终端字符画（TUI）/ SVG（web） | 自家 `@opencode/merman` / 真 `mermaid` npm | TS / MIT | 无 / 浏览器里跑 mermaid.js |

---

## 6. 给 Radian 的选项

前提（来自现有 ADR）：**ADR 0023** —— 绘制时不做 I/O、不做依赖终端应答的检测；**ADR 0028 R1** ——
条目→块→行，按条目缓存，流式只重解析正在写入的条目的最后一个块；**R2** —— 代码块带语言做语法
高亮；**ADR 0029** —— 主题是唯一颜色来源。

### 选项 A：不画（保持现状：mermaid 当普通代码块）

- 依赖：无。复杂度：零。
- 流式：无影响（R1 已处理）。80 列：源码换行。
- pty：现有测试已覆盖。
- 代价：R8 的答案就是 “off”，用户看不到图。**诚实且完全合规的默认值**。

### 选项 B：自己写一个**有界子集**渲染器（照 codex 的做法）

- 依赖：`unicode-width`（± `ascii-dag` 做布局）。复杂度：**高**（parser + 分层布局 + 边路由 +
  框线绘制；codex 那份 `codex-rs/mermaid` 源码只 172 KB，已是五个家族、上限 16 节点/24 边）。
- 流式：**只画已闭合的 fence**（codex 的 `has_closing_fence`），闭合前显示源码 → 与 R1 的
  “settled 前缀冻结、只重算尾部块”天然一致；不会像 omp 那样“图重排了已沉降的行”（omp 只能整段
  不沉降）。
- 80 列：`TooWide` → **保留源码 + 一行 dim 理由**（codex 的三种理由文案）。
- pty：**最好**。渲染是 `(source, width, theme)` 的**纯函数**（codex 的
  `render_source` 就是这种签名，测试逐块断言相等）→ 快照测试 + pty 测试都好写。
- ADR：完全合规 —— 无 I/O、无终端探测、颜色只来自主题（且自家 `Role::{Node,Edge,Text}`
  正好给 ADR 0029 一个映射点）、结果可直接进 R1 的块缓存。

### 选项 C：用现成 Rust crate（`mermaid-text` / `merman-ascii` / `figurehead`）

- 依赖：一个 crate + 它的依赖。复杂度：**低**（`render_with_width(src, Some(width))`）。
- **硬约束：MSRV**。Radian 是 **1.88**；`mermaid-text` 0.56+ 声明 **1.92**、`merman`/`merman-ascii`
  声明 **1.95**、`orthodag` 声明 **1.96** → **都不能用**，除非降到 `mermaid-text` **0.11.x**（声明
  1.85）或选 `figurehead`/`selkie-rs`（未声明 MSRV，需实测）。**0.11.x 的功能集比 0.57 小得多**
  （上面那张特性表是 master 的），具体支持到哪些家族 **未核实**。
- 颜色：这些库输出**纯文本、无 ANSI、也没有 span/role** → 只能整体上色或不上色；
  想要“节点一个色、边另一个色”必须改用给出角色的库（`merman-ascii` 是否给 **未核实**）。
  这一点直接决定 ADR 0029 能不能完整落地。
- 流式：同样**只画闭合的 fence**（crate 对半成品返回 `Err` → 显示源码）。
- 80 列：`render_with_width` 会压缩间距；但 README 自述“**很长的标签在很窄的列里可能重叠**”。
- pty：可测（纯函数），但要跟 crate 的发布节奏走。

### 选项 D：外部程序（`mmdc` / `d2` / `termaid`）

- 依赖：运行时要装一个程序（可能不存在）。复杂度：中。
- **与 ADR 0023 的冲突**：只能在**块闭合时**由 host 起子进程（当成一个 `Effect`），结果缓存在块里；
  绘制时不 I/O ✓。但引入了“子进程 / 沙箱 / 可用性”三个新问题，而且返回前只能显示源码。
- 80 列：看工具是否支持 `--width`。pty：要“装了才测”，CI 上易碎。
- 结论：不作为首选；若要做，也必须**默认关**。

### 推荐

**推荐 B（自写有界子集渲染器，先做 flowchart + sequence，照 codex 的形状）：**

1. **MSRV 安全**（只依赖 `unicode-width`，与 codex 同）；
2. **与 ADR 0023/0029 零冲突**：纯函数、无 I/O、无终端探测、角色→主题映射；
3. **与 R1 完美契合**：只画闭合的 fence，缓存键 `(source, width, theme)`，封闭块不重算；
4. **pty 可测**：快照 + 现有 pty 套件都能直接断言屏幕文本；
5. 不会把“图重排了已沉降的行”这种问题引进来（omp 为此刻意关掉了整段沉降）。

如果只想要最小代价，**退而求其次选 C 的 `mermaid-text` 0.11.x**（MSRV 1.85）——但要接受
“**没有颜色角色**”和“升级即撞 MSRV”两条代价；这两条在写 ADR 0028 R8 时要显式写清。

**流式建议（四家对比后的结论）**：跟 **codex**（只画闭合 fence），不跟 pi/omp/opencode 的实时渲染。
理由：Radian 的 R1 已经把“尾部块可变、已沉降前缀冻结”定下来了，实时重排图会直接破坏这条性质；
codex 还把“变窄了就把图换回源码”写成了显式行为，正好是 80 列下的安全退化。

**80 列下的行为**：`TooWide` → 显示源码 + 一行 dim 理由（不静默、不截断成乱码）；
以后若想要 omp 那种“换个方向重排”，那是第二阶段的事。

### 本节的未核实项（写 ADR 时需标出）

- `merman`/`merman-ascii` 是否给出 span/role（决定颜色能不能分元素）、以及有没有低 MSRV 的旧版本。
- `mermaid-text` 0.11.x 实际支持哪些家族、以及它的 MSRV 1.85 声明是否真实。
- `figurehead`/`selkie-rs` 的 MSRV、维护状态、输出质量（下载量小）。
- `graphviz-rust` 的许可（crates.io 写 non-standard）。
- 各家 crate 的依赖树（只看了 `mermaid-text` 的 `Cargo.toml`）。

---

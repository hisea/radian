# 调研：pi / omp / codex / opencode 怎样生成与显示图表

日期：2026-10-13 · 分支 `pi/research-diagrams` · 状态：**进行中**（结论文档，不改产品代码）。

结论会写回 ADR 0028 R8。每条附出处（`文件:行号`，或 URL + 访问日期 + 原文短引用）；查不到的写
「未核实」。**本文档目前只完成了 pi、omp 与库调查的一部分；codex / opencode 与 Rust 库调查、
给 Radian 的选项见文末「进度与下一步」。**

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

## 进度与下一步（本片未完成的部分）

已核实：**pi（生成/显示/流式/失败/库）**、**omp（同上 + 与 pi 的差异）**、
**`grok-mermaid` 的能力与上限**。

**未做（下一步按顺序）**：

1. **codex**（`openai/codex`，Rust `codex-rs/tui`）：查它的 Markdown 渲染对 mermaid/图表有没有处理；
   生成侧查系统提示是否提 mermaid。**未核实**。
2. **opencode**（`sst/opencode`）：同上。**未核实**。
3. **Rust 侧的现成库**（把 mermaid 至少 flowchart + sequence 渲染成终端字符画）：
   - 候选需要查：`mermaid-rs`（是否存在/状态）、`mermaid-ascii` 的 Rust 移植、`d2`/`graphviz` 的
     Rust 绑定（`graphviz-rust`）、`layout-rs`（Rust 的 graphviz 移植）、以及 `termgraph`/`textplots`
     这类"图表"库（后者是数据图不是结构图，只能作为旁证）。每条要写：名字、许可、维护状态、
     支持的图类型、是否纯本地、**访问日期**。**未核实**。
4. **最后一节：给 Radian 的 2–4 个选项**（依赖 / 体积与复杂度 / 流式怎么办 / 80 列效果 /
   pty 可测性），并按 ADR 0023（绘制不做 I/O、不做终端应答探测）、ADR 0028 R1（块模型与缓存）、
   ADR 0029（主题是唯一颜色来源）标注冲突点，最后写推荐与理由。**未写**。

**已知要注意的点（写选项时用）**：
- `grok-mermaid` 与 omp 的 vendored 渲染器都是 **TypeScript**；Radian 是 Rust，要么找 Rust 实现，
  要么把"渲染"放在宿主侧用别的进程/库（与 ADR 0023 的"绘制不做 I/O"冲突），要么只做**块显示 +
  可选外部命令**（如 `mmdc`/`d2`）——都要在选项里写清代价。
- `colorMode: "auto"` 会**探测终端 ANSI 能力**；ADR 0023 明确不做依赖终端应答的检测，所以
  Radian 只能固定用主题色（ADR 0029）——这点要在选项里标出。
- pi 的 `streaming` 与 omp 的"mermaid 存在就整段不沉降"说明：**流式期间图表会重排**是两家都
  承认的难点；Radian 的 R1（块模型 + 按宽度键控缓存）正好可以把它做成"最后一个块重算，封闭块
  不重算"。

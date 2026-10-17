# 调研：mermaid 框线图的渲染器该用哪个（2026-10-16）

状态：结论文档，不改产品代码。背景：否决了「去掉框线图」（ADR 0041 已否决记录），要求「找优化的方法」，并问两件事：(1) vendored 的 `codex-mermaid` 是不是最好的选择；(2) 用了 crate 还要自己写 layout（ADR 0028）是不是最好的集成方案。

方法：同一组 8 张样例（6 节点 TD 链、菱形分叉合并带回边、LR 链、12 节点的 radian crate 图、4 参与者时序图、状态图、subgraph、长标签 LR）在 80 列和 120 列下，分别用五个渲染器实际渲染。完整输出在 `2026-10-16-mermaid-renderer-candidates/outputs.txt`，样例源码在同目录 `samples.rs`。耗时是 debug 构建的单次测量，只看数量级。

## 结论

1. **`codex-mermaid` 不是最好的选择。** 它把所有节点排成一列、每条边占一条侧面通道（修订 6 的背景已核实这是设计），4 个参与者的时序图在 80 列就「太宽」，不支持 subgraph，标签上限 40 格；8 张样例里 3 张报错、2 张画成一列长图。它的唯一优点是小（2425 行，零依赖）。
2. **「vendor 一个渲染器再自己写一层布局」是错的集成方式。** 修订 6 的 1322 行只接住了「链」这一种形状：菱形分叉、12 节点图都退回 codex 的一列式布局。要让 flowchart 普遍好看，缺的是 Sugiyama 分层布局（去环、分层、减少交叉、坐标、正交走线），这是一整个算法，不该在 vendor 旁边补丁式地长出来。
3. **有一个现成的 Rust 原件，就是 pi 和 dsh-TUI 在用的那套算法的出处。** `xai-org/grok-build`（Apache-2.0，公开，27k star，2026-09-29 仍在推）的 `crates/codegen/xai-grok-markdown/src/mermaid.rs`：单文件 5699 行，只依赖 `ratatui`（`Style`/`Line`/`Span`）与 `unicode-width`，文件内含 145 个测试，edition 2024（let-chains），**在 Rust 1.88 上原样编译通过**（本机 `cargo +1.88 build`）。pi 的 `grok-mermaid` 0.2.3 和 dsh-TUI 的 `lovely-mermaid` 0.3.3 都是它的 TypeScript 移植（lovely-mermaid 的 README 自述「byte-faithful TypeScript port」，本次 8 张样例两者输出逐字节一致）。支持 flowchart（含 subgraph、六种边头、三种形状、标签自动换行）、sequence、state、class、ER；上限 128 节点 / 512 边；太宽或不支持时退回「带标题的源码框 + 一行提示」。
4. **推荐：用 grok-build 的 `mermaid.rs` 替换 `codex-mermaid` 加修订 6 的布局**，一次换到位。质量上与 pi、dsh-TUI 平齐（这正是 owner 要的 parity），代码量从 2425 + 1322 + 651（测试）行变成 5699 行一个文件（其中约 1650 行是测试），依赖不增（radian-tui 已用 ratatui 0.30，见下面「要核对」），许可相同（Apache-2.0）。

## 8 张样例的结果（80 列）

| 样例 | radian 今天 | grok-build `mermaid.rs` | mermaid-text 0.57 | merman 0.9 |
|---|---|---|---|---|
| 6 节点 TD 链 | 修订 6 布局，17×33，框间 3 行空隙 | 18×28，居中、框间 2 行 | 19×43，框间 5 行 | 17×56，框 5 行高 |
| 菱形分叉合并 + 回边 | 退回 codex：一列 6 框、36×40 | 31×18，分叉、合并、回边都对 | 37×34，可读但稀疏 | 31×36，可读，框 5 行高 |
| LR 链 | 修订 6 布局，44×3 | 47×4 | 50×3 | 50×6 |
| 12 节点 radian crate 图 | 退回 codex：一列 12 框、50×77 | 需要 129 列，80/120 都退回源码框 | 113×23，边线重叠、标签被切成两行 | 退回：**倒出 451 行模型字段**（Fallback 策略） |
| 4 参与者时序图 | **80 列报错**；120 列 85×31 | 52×35，紧凑 | 69×27 | 55×34 |
| 状态图 | codex 一列式 37×35 | 34×18 | 75×12，**边线穿框、标签错位** | 23×34 |
| subgraph | **不支持** | 8×15 | 9×23 | 9×30 |
| 长标签 LR | **超限报错** | 需要 84 列：80 列退回源码框，120 列 84×6 标签自动换行 | 86×5 | 退回：倒出模型字段 |
| 耗时（debug） | 0.03–0.3 ms | 0.07–0.7 ms | 0.1–20 ms | 12–740 ms |

读法：grok-build 的渲染器在 6/8 张上给出的是五个里最紧凑、最像 mermaid.js 的图；它不做「压扁适配宽度」，太宽就老实退回源码框（pi 一样）。omp 的做法是太宽时把 LR 换成 TD 重排（见 `2026-10-13-terminal-diagrams.md` §2.2），这是替换后可以加的一个小改进，不在本文范围。

## 候选逐个说

| 候选 | 许可 | MSRV | 依赖 | 能否分角色上色 | 判断 |
|---|---|---|---|---|---|
| `codex-mermaid`（现 vendor，pin 2f05c0a） | Apache-2.0 | 1.88 ✓ | 0 | 是（Node/Edge/Text） | 布局一列式；上游 2026-10-13 后只有一个提交（节点标签换行），没有改布局 |
| ADR 0028 自写布局（4881312） | — | — | — | 同上 | 只覆盖链与简单分层；要补齐等于再写一个 Sugiyama |
| **grok-build `mermaid.rs`**（4827113，2026-09-15） | Apache-2.0 | 1.88 ✓（edition 2024） | ratatui、unicode-width | 是（5 个 `MermaidStyles` 角色：border、node_text、edge、edge_label、title） | **推荐** |
| `mermaid-text` 0.57.0 | MIT | 1.92 | ascii-dag、chrono、unicode-width | 否（只有按 mermaid `style` 指令的 truecolor，不是按角色） | 12 节点图线重叠、状态图错位；18 种图但质量不齐 |
| `merman` 0.9.0 + `merman-ascii` | MIT OR Apache-2.0 | 1.95 | 大（facade + core + ascii，默认带 ELK/EPL-2.0；只开 ascii 也要 serde、thiserror 等） | 有 `AsciiColorRole` | 定位是 mermaid.js 的 parity 实现（Zed 用它出 SVG）；终端输出框 5 行高、慢 100–1000 倍；退回时倒出几百行模型字段 |
| `ascii-dag` 0.11 | MIT OR Apache-2.0 | 1.92 | 0 | 有 | 只是布局底座，不认 mermaid；自己写解析 = 重走 codex 的路 |
| `figurehead` / `selkie-rs` / `orthodag` | MIT / MIT / dual | 未声明 / 未声明 / 1.96 | 多 / 多 / 少 | — | 下载量小或 MSRV 太高，未实测 |

## 其他 harness 用什么（2026-10-16 核对本机副本）

| harness | 渲染器 | 算法出处 |
|---|---|---|
| pi 1.1.0 | `grok-mermaid` 0.2.3（npm） | grok-build `mermaid.rs` 的 TS 移植 |
| dsh-TUI 0.15.0 | `lovely-mermaid` 0.3.3（npm，grok-mermaid 改名后） | 同上 |
| omp 17.3.8 | vendored `beautiful-mermaid` ASCII 管线 | Craft Docs，grid 布局 + A* 走线 |
| codex | 仓内 `codex-mermaid` | 自写，一列式 |
| opencode 2.0.30 | 自写 `@opencode/merman`（约 12.9k 行 TS） | 自写 |
| Claude Code 2.1.301 | 内建 mod `cc-plugin-mermaid`（flag 后） | 未知（二进制） |

六家都只在显示端渲染，没有一家给模型工具或 skill。radian 的 `mermaid_check` 与 `diagrams` skill 是 dogfood 教训（ADR 0025）；换渲染器后 skill 正文要按新渲染器的子集与上限重写，工具保留。

## 换成 grok-build 渲染器要核对 / 要决定的

- **ratatui 版本**：grok-build 用 0.29，radian-tui 用 0.30.2。`mermaid.rs` 只用 `Style`、`Modifier`、`Line`、`Span`，这几个类型 0.29 → 0.30 没变（未逐项核实，实施时编一下就知道）。`radian-host` 今天不依赖 ratatui，`mermaid_check` 要么让 host 也依赖 ratatui（重），要么把文件里的 `ratatui::text::{Line,Span}` 换成一个自带的最小 span 类型（约 77 处引用，是一次性的、可记录的修改）。后者更干净，但违反 R8「vendor 原样不改」——需要 定：允许「有记录的最小改动」。
- **许可**：Apache-2.0，与 codex-mermaid 相同；仓库根有 `LICENSE` 和 `THIRD-PARTY-NOTICES`，vendor 时照 R8 的做法附上。
- **上游节奏**：该文件 5 次提交（2026-07-16 开源到 2026-09-15），都是「Synced from monorepo」，没有 issue 级的沟通渠道；升级靠重拷贝加审 diff，与今天一样。
- **退回文案**：原文「open the image to view it in full」是 Grok 有图片路径；radian 没有，退回框的提示句要改成自己的（例如「宽度不够：需要 N 列」），这是渲染层外面一行文字，不改 vendor 也能做（渲染层拿到 `too_wide` 后自己画框）。
- **ADR**：这是 ADR 0028 R8 的「移植 codex-mermaid」与修订 6 两条决定的替换，要新 ADR 或 0028 修订 7；修订 6 的 1322 行布局与 651 行测试随之删除。

## 未核实

- grok-build `mermaid.rs` 在 ratatui 0.30 下是否零改动编译（只在 0.29 下编过）。
- `MAX_LABEL = 28`、`WRAP_WIDTH = 24` 对中文标签的效果（样例全是英文）。
- 流式：该渲染器是纯函数、没有「只画闭合块」的概念，闭合判断仍在 radian 这边（与今天相同）。

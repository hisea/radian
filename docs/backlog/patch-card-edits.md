# `patch` 卡片不认 `edits` 形式：头部恒为 `+0 −0`，展开没有 diff

- 状态：decided（可以派发；一个 worker，`radian-coding` 的结果形状加一个字段，`radian-tui` 的卡片跟上）
- 涉及 ADR：0028（R4 工具卡片：`patch` 头部的 `+N −M`；Diff 一节：`hunks` 的形状与 `@@` 行的合成）
- 来源：arc worker 会话的 transcript：连续五次 `patch` 同一文件，卡片头部全是 `+0 −0`，展开只剩一行 "replaced 4 occurrences across 4 edits"，从界面上看不出改了什么，看的人会以为编辑在空转

## 背景

`patch` 有两种调用形式：老的 `old`/`new` 一对，和新的 `edits` 数组（一次调用对一个文件做多处原子替换）。工具本身两种都正确执行，结果文本也对。

TUI 的卡片只认老形式。`crates/radian-tui/src/tools/cards.rs` 里：

- `diff_meta` 只数 `arguments.old` 和 `arguments.new` 的行数。`edits` 形式下两者都不存在，头部恒为 `+0 −0`。
- `patch_diff_text` 取不到 `old`/`new` 就返回 `None`，卡片体退回工具的一行结果文本，没有 `-`/`+` 行。

模型现在默认用 `edits` 形式（一次 Clock 穿参重构在 43 秒内发了五次 `patch`，每次 1 到 4 处），所以这不是边角：今天大部分 `patch` 卡片都是瞎的。

还有一个形状上的缺口：结果里的 `hunks` 按**文件顺序**摊平，而 `edits` 按**调用顺序**给出 old/new 文本，两者之间没有对应关系。一个 edit 还可能匹配多处（每个 edit 有自己的 `replace_all`），所以 hunk 数可以大于 edit 数。卡片要把每个 hunk 配上正确的 `-`/`+` 文本，结果必须说明每个 hunk 来自哪个 edit。

## 决定（已定）

### 1. 结果的每个 hunk 带上来源 edit

- `radian-coding` 的 `patch` 结果里，`hunks[]` 的每一项加 `"edit": <n>`，n 是 1-based 的 edit 编号，与同一结果里 `results[].edit` 一致。老形式也带（恒为 1），两种形式的 hunk 形状相同。
- `hunks` 仍按文件顺序摊平，不改排序；`results`、`replaced`、`matched`、结果文本都不变。
- ADR 0028 Diff 一节那句 `"hunks": [{old_start,new_start,old_lines,new_lines}]` 改为含 `edit`，并加一句：卡片按 hunk 的 `edit` 取该 edit 的 `old`/`new` 文本画 `-`/`+` 行。docs-only commit。

### 2. 卡片对两种形式一视同仁

- `cards.rs` 新增一个"本次调用的 edit 列表"读取：老形式是一项 `(old, new)`，`edits` 形式是数组里每项的 `(old, new)`。`diff_meta` 和 `patch_diff_text` 都只走这个列表，不再直接读 `arguments.old`。
- 头部 `+N −M`：有结构化 `hunks` 时，N 是所有 hunk 的 `new_lines` 之和，M 是 `old_lines` 之和（这样一个 edit `replace_all` 匹配三处也数三次，比今天只数一次调用文本更准）。没有 `hunks`（老 journal 条目）时退回今天的算法：edit 列表里每项 old/new 行数求和。
- 卡片体：按文件顺序逐个 hunk 画 `@@ -a,b +c,d @@`，然后用该 hunk `edit` 指向的那个 edit 的 old 文本画 `-` 行、new 文本画 `+` 行。没有 `hunks` 时按 edit 顺序逐个画，`@@` 行省略，和今天老条目的退化方式一样。hunk 的 `edit` 越界或缺失时，按 edit 顺序退化，不 panic。
- 卡片头部标题仍是路径；`collapsed` 的 `· N lines` 按合成后的 diff 行数算，和今天一样。
- 复制与搜索的不变量不变：卡片体的文本就是条目文本（`result_display_text`）。

## 验收

- `radian-coding`：`edits` 两项、其中一项 `replace_all` 匹配两处的调用，结果 `hunks` 有三项，`edit` 分别对应正确的 edit 编号且按文件顺序排；老形式的 hunk 带 `edit: 1`。钉住结果形状的现有测试同步。
- `radian-tui` `tools.rs` 的卡片测试：`edits` 形式的卡片，`meta_parts` 给出按 hunk 求和的 `+N −M`；`result_display_text` 给出每个 hunk 一段 `@@` + `-`/`+` 行，顺序为文件顺序，文本来自正确的 edit；无 `hunks` 的 `edits` 条目退化为按 edit 顺序、无 `@@`；老形式的现有测试不变。
- 变异：`diff_meta` 改回只读 `arguments.old`；hunk 的 `edit` 全部当作 1 处理。各有测试失败。
- `docs/user/` 里描述 `patch` 卡片或 `hunks` 的地方（如有）同步；ADR 0028 的改动过 docs drift 测试。

# 上下文文件的 `@path` 导入

- 状态：idea
- 涉及 ADR：0036（上下文文件的发现、上限、不可信、仓库外不读）
- 来源：`docs/research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §5.4、§6 ⑤

## 背景

- Claude Code 的 `CLAUDE.md` 支持 `@path` 导入：相对所在文件解析，最多 4 层，跳过代码块，导入仓库外的文件要先确认。omp 按同样语义展开。
- Radian 的 `crates/radian-host/src/context.rs` 不展开导入（2026-10-20 对 `main` 核实，没有导入逻辑）。一个只有 `CLAUDE.md`、正文是 `@docs/conventions.md` 的仓库，模型拿到的就是这一行字面文本；它也许会自己去读那个文件，但多一轮，也可能不读。
- 本仓库这种 `CLAUDE.md` 只写 `@AGENTS.md` 的情况不受影响，因为 ADR 0036 的发现顺序先选中 `AGENTS.md`。

## 方向（研究报告建议，owner 未定）

- 在 `context.rs` 里照 Claude 文档的语义展开：`@` 必须在行首或空白之后；相对路径按所在文件的目录解析；最多 4 层；跳过行内代码和围栏代码块；循环导入只展开一次。
- **只展开仓库内的路径**：指向仓库外的导入保留原样，并在启动行列出（ADR 0036 不读仓库外的文件，不扩大读的范围）。
- 展开后的字节计入 ADR 0036 的 64 KiB / 128 KiB 预算；超出照旧先丢外层。
- 不做 `.claude/rules/*.md` 的 `paths:` 限定（需要"按文件加载"这套新机制）和 `CLAUDE.local.md`（Claude 私有约定）。
- 上下文文件本来就不进信任门，展开后的内容一样当不可信输入（ADR 0036）。约 100 行的纯函数，ADR 0036 补一段。

## 待决问题

- 只对 `CLAUDE.md` 展开，还是 `AGENTS.md`/`AGENTS.override.md` 也展开。`agents.md` 规范没有 `@` 导入；pi 和 Codex 是否展开未核实。
- 展开的文件要不要在启动行单独列出（`context: CLAUDE.md (2.1 KB, +docs/conventions.md 3.4 KB)`）。

## 验收

- `@docs/a.md` 被展开；写在反引号里的 `` `@README` `` 和围栏代码块里的 `@x` 不展开。
- 第 5 层的导入不再展开；循环导入只出现一次。
- 指向仓库外的导入不读，原样保留并在启动行列出。
- 展开后超出 128 KiB 时照 ADR 0036 丢外层。
- 变异检查：去掉深度上限，循环导入的 fixture 必须挂住超时（或断言展开次数）。
- 落地时同分支改 ADR 0036（docs-only commit）并删掉本文件。

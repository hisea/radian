# 上下文文件：子目录按需装载，按 glob 触发的规则

- 状态：decided（2026-10-31：第 1 条子树 AGENTS.md 按需装载；glob 规则不做）
- 涉及 ADR：0036（基础提示与上下文文件）、0025（skill 按描述触发）、0031（项目信任）
- 来源：`research/2026-10-31-competitor-memory.md` §2.2、§2.10（Amp）、§4

## 背景

radian 装载的上下文文件是全局文件加从仓库根到工作目录这条路径上的 AGENTS.md，外层在前（`docs/user/context-files.md`）。对手多了两种：子树按需，Amp 在模型读到某个子树里的文件时把该子树的 AGENTS.md 加进来，Gemini CLI 的子目录 GEMINI.md 同样按需；按 glob 触发的规则，Cursor 的 rules 有 Always / Auto-attached（glob）/ Agent-requested（描述）/ Manual 四种，Windsurf 的 `trigger: always_on | glob | model_decision | manual`，Copilot 的 `instructions/*.instructions.md` 带 `applyTo`。radian 的 skill 已经覆盖「按描述触发」这一种。

## 决定（已定）

- 子树 AGENTS.md：当 `read`、`grep`、`glob` 或 `patch` 的结果命中工作目录之下某个子目录里的文件时，把该文件所在目录到工作目录之间尚未装载的每个 AGENTS.md（含 `AGENTS.override.md` 这类 ADR 0036 已认的名字）作为一条标注来源路径的系统消息追加到转录，每个文件只追加一次；大小上限与 ADR 0036 现有的上下文文件上限同一套，超限截断并标明。装载的路径与大小记进 journal 的 live 事件（ADR 0010），不进启动行（启动行只有启动时装载的）。压缩后不再注入：它们是普通系统消息，按 ADR 0018 的规则进摘要输入。工作目录之外、仓库根之上、以及符号链接指向边界外的 AGENTS.md 不装载（ADR 0036 的边界规则）。
- glob 规则（`.radian/rules/<name>.md` 带 `paths:`）：不做；子树 AGENTS.md 覆盖同一需求，看一个月的装载数据再说。
- 关闭开关：`[context] subtree_files`（布尔，默认 `true`），`false` 时只装载启动时那一组。

## 验收

- 读 `crates/x/src/a.rs` 后，`crates/x/AGENTS.md` 作为一条系统消息出现一次，journal 有记录其路径与字节的 live 事件；再读同目录另一个文件或 `crates/x/src/b.rs` 不重复注入；读 `crates/x/sub/c.rs` 只追加 `crates/x/sub/AGENTS.md`（`crates/x/AGENTS.md` 已装载）。
- 启动时已装载的 AGENTS.md（根到工作目录）不会被再次追加。
- 一个超过上下文文件上限的子树 AGENTS.md：截断并标明，与启动装载的截断行为相同。
- `[context] subtree_files = false`：读子树文件不追加任何系统消息。
- 压缩后转录里不再有这条系统消息，但摘要输入包含过它。
- 手册 `context-files.md` 写出子树装载的触发、上限、开关；`configuration.md` 加 `context.subtree_files` 行；ADR 0036 在 docs-only commit 里把子树装载写成决定，并删掉本文件。

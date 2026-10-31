# 上下文文件：子目录按需装载，按 glob 触发的规则

- 状态：idea
- 涉及 ADR：0036（基础提示与上下文文件）、0025（skill 按描述触发）、0031（项目信任）
- 来源：`research/2026-10-31-competitor-memory.md` §2.2、§2.10（Amp）、§4

## 背景

radian 装载的上下文文件是全局文件加从仓库根到工作目录这条路径上的 AGENTS.md，外层在前（`docs/user/context-files.md`）。对手多了两种：子树按需，Amp 在模型读到某个子树里的文件时把该子树的 AGENTS.md 加进来，Gemini CLI 的子目录 GEMINI.md 同样按需；按 glob 触发的规则，Cursor 的 rules 有 Always / Auto-attached（glob）/ Agent-requested（描述）/ Manual 四种，Windsurf 的 `trigger: always_on | glob | model_decision | manual`，Copilot 的 `instructions/*.instructions.md` 带 `applyTo`。radian 的 skill 已经覆盖「按描述触发」这一种。

## 待决问题

- 子树 AGENTS.md：在 `read`/`patch` 命中一个子目录里的文件时，把该目录到工作目录之间尚未装载的 AGENTS.md 作为一条系统消息追加（一次，之后不重复）；装载的路径与大小照旧记进启动行。压缩后要不要再注入，按 ADR 0018 的系统消息规则。
- glob 规则：是否值得单独一种文件（例如 `.radian/rules/<name>.md`，frontmatter `paths:`），还是让子树 AGENTS.md 覆盖同一需求。倾向先只做子树 AGENTS.md，看一个月的装载数据再说。
- 大小上限与 ADR 0036 现有的上下文文件上限同一套。

## 验收（落地时）

- 读 `crates/x/src/a.rs` 后，`crates/x/AGENTS.md` 出现在上下文里一次，启动行之外的日志记录了它；再读同目录另一个文件不重复注入。

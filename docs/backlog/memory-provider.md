# memory 改成宿主 context hook + 外部 provider

- 状态：idea
- 涉及 ADR：0021、0043
- 来源：`docs/research/2026-10-18-area-deep-dive/9-long-tasks-memory.md` §6.5；总报告第 21 条

## 背景

测量用的三个会话 479 次工具调用里 memory 工具出现 0 次；三个工具定义约 2.5 KB 常驻上下文。`memory_save` 默认 ask，按项目放行也没有出口。

## 待决方向（lead 建议，未定）

去掉默认集里的三个模型侧 memory 工具；memory 变成宿主的 context hook：`inject`（会话开始 + 每轮用户消息，沿用今天 ADR 0021 D6 的逻辑）与可选的 `capture`（轮末，只读已提交的会话事实，默认关）。JSONL 存量是内置 provider；外部 provider 走子进程协议（stdin JSON → stdout block），只允许用户级配置（与外部命令 hook 一起设计）。"skill + `radian memory` CLI 跑在 shell 里"不可行：宿主分不清 agent 起的和用户起的 CLI（ADR 0021 不变量 3、6），沙箱也写不了记忆库（不变量 5）。

## 待决问题

- （A）模型完全不能主动写，还是（B）留一个可选的 `remember` 工具（save+revise 合并，默认关）？Lead 推荐 A，等 定。

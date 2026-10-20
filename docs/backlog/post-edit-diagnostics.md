# 编辑后诊断

- 状态：idea
- 涉及 ADR：新 ADR
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P3；总报告第 17b 条（tree-sitter 语法层 + 按 glob 配置的检查命令）

## 背景

改完要再跑一轮 `shell cargo check`，纯多一轮往返。`agent-core` 已有 `after_tool_call` hook 可挂，不用改 Core。

## 待决方向

不做新 tool，由宿主在 `patch`/`write` 成功后自动跑：第 1 层内置 tree-sitter 语法检查（零配置、默认开、不执行仓库代码，不碰项目信任）；第 2 层按 glob 配置的检查命令（默认关，自动探测只建议不启用）。更便宜的中间方案是先只做通用的 `post_tool` 用户命令 hook。

## 验收

先数数据：dogfood 里 patch→`cargo check` 序列有多少；评测集上比较每个任务编辑后检查的调用数与总轮数。

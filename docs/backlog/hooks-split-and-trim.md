# command hooks：按使用证据瘦身

- 状态：idea
- 涉及 ADR：0045、0008、0047、0031
- 来源：2026-11-21 精简调查

## 背景

`radian-host/src/hooks/`（原来是单个 7000 行的 `hooks.rs`，已按职责拆成模块，行为不变）装着六个事件的解析与执行、async/asyncRewake、"Recent runs" 记录环、`Stop` 上限提示等；另有 `project_hooks.rs`、`config/hooks.rs`。operad 依赖 `PreToolUse`（做只读和权限限制），`/goal` 和轮末续跑依赖 `Stop`。

## 待决问题（未定）

1. 瘦身：`async`/`asyncRewake`、`StopFailure`、`Notification`、`PermissionRequest`、"Recent runs" 是否有人用。先查 owner 和 operad 的配置及 journal 里的 hook 记录，再逐项决定删或留；`PreToolUse`、`PostToolUse`、`Stop` 不在讨论范围内。删除的事件按 ADR 0045 现有规则"跳过并记为 skipped"，不拒绝配置。

## 验收

另行决定后补。

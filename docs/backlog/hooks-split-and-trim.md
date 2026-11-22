# command hooks：拆文件，再按使用证据瘦身

- 状态：decided（1）；idea（2）
- 涉及 ADR：0045、0008、0047、0031
- 来源：2026-11-21 精简调查

## 背景

`radian-host/src/hooks.rs` 约 296 KB、7000 行，是全仓最大的单个源文件（约 60% 是生产代码），装着六个事件的解析与执行、async/asyncRewake、"Recent runs" 记录环、`Stop` 上限提示等；另有 `project_hooks.rs`、`config/hooks.rs`。operad 依赖 `PreToolUse`（做只读和权限限制），`/goal` 和轮末续跑依赖 `Stop`。

## 决定（已定）

1. 按事件和职责把 `hooks.rs` 拆成模块（解析、执行与超时、各事件的输出解释、运行记录），纯移动，行为和测试不变。

## 待决问题（未定）

2. 瘦身：`async`/`asyncRewake`、`StopFailure`、`Notification`、`PermissionRequest`、"Recent runs" 是否有人用。先查 owner 和 operad 的配置及 journal 里的 hook 记录，再逐项决定删或留；`PreToolUse`、`PostToolUse`、`Stop` 不在讨论范围内。删除的事件按 ADR 0045 现有规则"跳过并记为 skipped"，不拒绝配置。

## 验收

1：`hooks.rs` 不再是单个大文件，`command_hooks` 测试与 Verify 全绿，无行为变化。2：另行决定后补。

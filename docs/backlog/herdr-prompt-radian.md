# `herdr agent prompt` 拒认运行中的 radian（待查）

- 状态：idea（bug 待查）
- 涉及 ADR：0034
- 来源：lead 2026-10-19 实测

## 背景

`herdr agent prompt <pane>` 对运行中的 radian TUI 报"is not an active named agent"。2026-10-17 构建与 `aaf1502` 构建（`herdr pane run` 启动）都复现。ADR 0034 说 radian 是 herdr 一等自报 agent。

## 待决问题

原因未知：启动方式、插件、还是上报 bug。先查再定修不修、怎么修。

# `explore` 子会话用更便宜的模型

- 状态：idea
- 涉及 ADR：0043（§1 已允许）、0026
- 来源：总报告第 14 条；`4-architecture.md` §5.2；评估

## 背景

0043 §1 写了 `explore` "可在用户级配置里给它更便宜的模型"，但没有实现：子进程一律拿父会话此刻的路由（`crates/radian-host/src/subagent.rs` 的 `current_route`），配置 schema 里也没有这一项。对手都有：Claude Code 的 Explore 可换 haiku，omp 有 `--smol` 和 `--slow` 两个模型槽。

## 待决方向

用户级配置加一项 `explore` 的 provider/model；未设置时照旧跟父。只做 `explore`，`general` 跟父不变。压缩用小模型是另一条（`cache-efficiency.md` 不含，需要时另开）。

## 验收

设置后 `explore` 子会话的 `SessionCreated` 路由是配置里的模型，`general` 仍跟父；未设置时两者都跟父；父 `/model` 切换后 `general` 跟着变、`explore` 不变。

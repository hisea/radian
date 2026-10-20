# orca：替代 herdr 的派发与闸门（v0）

- 状态：idea＼�设计定为 v0，记录在案，**不开工**；先仓库外原型，不进二进制）
- 涉及 ADR：0032（保留子命令；落地时要加 `orca`/`work`）、0043（远期 `radian orca`；`subagent` 是子进程的先例）、0044（`--output jsonl` 的 `Result` 与退出码是 worker 的回传契约）、0024（`human` 闸门是结构化交互的宿主外映射）、0013（`orca serve` 只在 loopback/ssh 转发，不出网）、0037（stall 超时按无输出计，与后台命令的完成通知同源）、0022（返工 `--resume`，不 fork）
- 设计全文：`docs/research/2026-10-20-orca-v0-design.md`
- 对手调查：`docs/research/2026-10-20-orca-competitors.md`

## 决定＼�

- Claude 是 lead：分析需求、拆 item、经 orca 的 MCP 工具写入 backlog、看判定、定返工；merge 归 owner。
- orca 是确定性程序，不是模型：**文件 backlog 的唯一写者**（`todo/doing/review/done/failed`，rename 即状态），不依赖 git 分支，不长驻（`orca run` 排空即退）。
- **一个 work item 一个 session**：session id = item id，返工 `--resume` 回同一台机器；会循环的只有脚本和定时器。
- worker 是一次性 `radian exec -p`，只读、无输入口；人类用 `radian attach` 观察。
- v0 推（ssh + stdin），v1 拉（`radian work --pull`，需要 `orca serve`）；`radian work` 两种都接。
- 闸门是可配置阶段序列：`commands` / `mutation` / `review` / `ci` / `human`，brief 只能加严；`on_fail` 按阶段配 `rework` 或 `hold`。
- v1 要 desktop / iOS / web app：v0 的 backlog 文件格式就是 API，要写 schema；`run` 与 `serve` 共用状态机。

## 待决

- 原型语言与位置（仓库外的脚本目录，还是独立 crate）。
- brief 里 `mutations:` 的声明格式。
- `--session-dir` 跨机器 resume 是否可行（未验证）。
- `radian attach` 的输出是否已是 jsonl（v1 代理观察流需要）。
- 落地时用保留的 `serve` 还是新名 `orca`（改 ADR 0032）。

## 验收（落地时）

- 一个真实需求从 Claude 拆分到 `done/`，全程没有 herdr；每个 item 恰好一个 session 文件；返工后 `Result.turns` 递增而 session id 不变。
- `commands` 与 `mutation` 从不同树、同机同时；`human` 停下后 `pass`/`fail` 推进状态。
- orca 被杀后重启，`doing/` 里的 item 不丢、不重复派发。
- 落地同分支：新 ADR、改 0032/0043（docs-only commit）、删本文件。

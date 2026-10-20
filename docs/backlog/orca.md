# orca：替代 herdr 的派发与闸门（v0）

- 状态：idea＼�设计定为 v0，记录在案，**不开工**；先仓库外原型，不进二进制）。
  2026-10-20 外部审核的 P0 六条已采纳进设计稿；三处分歧 owner 保留原决定。
- 涉及 ADR：0032（保留子命令；落地时要加 `orca`/`work`）、0043（远期 `radian orca`；`subagent` 是子进程
  的先例；缺省 `full`，无人值守必须显式 `--profile workspace`）、0044（`--output jsonl` 的 `Result` 与退
  出码是 worker 回传契约；`completed` 不等于任务完成；`cost` 为会话累计）、0038（`workspace` 沙箱内
  worktree commit 的 `.git` 写边界，未验证）、0024（`human` 闸门是结构化交互的宿主外映射）、0013
  （`orca serve` 只在 loopback/ssh 转发）、0037（stall 超时按无输出计）、0022（返工 `--resume`，不 fork）
- 设计全文：`docs/research/2026-10-20-orca-v0-design.md`（修订版）
- 外部审核：`docs/research/2026-10-20-orca-v0-review-and-competitor-analysis.md`
- 对手调查：`docs/research/2026-10-20-orca-competitors.md`

## 决定＼�

- Claude 是 lead：分析需求、拆 item、经 orca 的 MCP 工具写入 backlog、看判定、提交 rework brief；
  **批准（pass/fail）是 owner 权限，不在 MCP 工具里**；merge 归 owner。
- orca 是确定性程序，不是模型：文件 backlog 状态变更的**唯一写者**（`todo/doing/review/done/failed`
  目录为粗状态，frontmatter `phase` 为细状态），短事务锁 + `state_version`，不依赖 git 分支，不长驻
  （`orca run` 推进到没有立即可做的事就退；human、CI、退避等待不堵它）。
- 三个身份：`item_id`；`session_id = item_id`（**一个 work item 一个 session**，返工 `--resume` 回原
  host，不可恢复进 `failed`，人重新 add）；`attempt_id` 每次执行一个，旧 attempt 的结果被拒。
- worker 是一次性 `radian exec -p --profile workspace --yes --output jsonl`，只读、无输入口；host 侧的
  `radian work` 包装器在模型退出后核 commit/dirty/sha 并 push，模型不持有 push 凭据。
- 交付核验由 orca 机器做（base/head sha、dirty、远端 ref、有效改动），不信 `Result.outcome`。
- 远端状态未知时进 `reconciling`，不自动重派；失败分四类，只有基础设施与模型失败自动退避重试；预算
  `max_attempts/max_cost/max_wall_time` 到顶进 hold。
- `done` 只等于已合并；gate 全过只到 `awaiting_human`。verdict 绑 sha，分支动了作废；main 动了合并前复验。
- 闸门是可配置阶段序列：`commands` / `mutation` / `review` / `ci` / `human`，brief 只能加严；命令来源可
  信。通用缺省 `commands → human`；**radian 仓库 profile 加 `mutation:required`**。
- v0 推（ssh + stdin），v1 拉（`radian work --pull`，需要 `orca serve`）；`radian work` 两种都接。
- v1 要 desktop / iOS / web app：v0 冻结 identity、state、attempt、verification sha 四组字段并写
  `schema_version`；`run` 与 `serve` 共用状态机；UI 读 orca 存的 jsonl 流，不解析 attach。

## 待决

- 原型语言与位置（仓库外的脚本目录，还是独立 crate）。
- brief 里 `mutations:` 的声明格式。
- 远端进程身份用什么（session 文件锁、pid+starttime、包装器写的 token 文件）。
- `workspace` 沙箱内 worktree 能否 commit（ADR 0038）；jsonl 流在模型长思考时是否持续有行。
- 落地时用保留的 `serve` 还是新名 `orca`（改 ADR 0032）。

## 验收（落地时）

- 一个真实需求从 Claude 拆分到 `done/`，全程没有 herdr；每个 item 恰好一个 session 文件；返工后
  `Result.turns` 递增而 session id 不变，attempt 目录递增。
- `commands` 与 `mutation` 从不同树、**不同时**；`human` 停下后 `pass <item> <sha>`/`fail` 推进状态，
  sha 不符被拒。
- 审核报告第 10 节的故障注入全部通过：两个 `orca run` 并发只一个能 claim；ssh 断而远端仍跑时不重派；
  同 pid 复用不被认成旧 attempt；exit 0 但 dirty 或无产出不过；gate 验过 sha A 后分支推到 B 旧 verdict
  作废；A 合入后 B 复验；预算耗尽停下；等人时 `run` 能退且下次恢复。
- 落地同分支：新 ADR、改 0032/0043（docs-only commit）、删本文件。

# arc：替代 herdr 的派发与闸门（v0）

- 状态：idea（设计定为 v0，记录在案，**不开工**）。落地时直接进 radian 二进制：`radian arc` 不带子命令打开 TUI，`radian arc <子命令>` 是 CLI（`add / list / show / start / merge / reject / auto / run`）；不做仓库外原型，不出独立二进制。
  两轮外部审核的 P0 已全部采纳进设计稿；第一轮三处分歧保留原设计，第二轮保留一处：单机 v0 用 linked worktree 共享 clone，不做独立 clone。
- 涉及 ADR：0032（保留子命令；落地时要加 `arc`/`work`）、0043（`radian arc` 子命令，落地前先改 ADR；`subagent` 是子进程
  的先例；缺省 `full`，无人值守必须显式 `--profile workspace`）、0044（`--output jsonl` 的 `Result` 与退
  出码是 worker 回传契约；`completed` 不等于任务完成；`cost` 为会话累计）、0038（`workspace` 沙箱内 git 可写 refs/index/objects、只有 config/hooks 只读；linked worktree 共享
  common dir；单机 v0 仍用 worktree，靠包装器检测并安全失败）、0020（push 凭据的禁读清单，同一 OS 用户下是否遮住待验证）、0024（`human` 闸门是结构化交互的宿主外映射）、0013
  （`arc serve` 只在 loopback/ssh 转发）、0037（stall 超时按无输出计）、0022（返工 `--resume`，不 fork）
- 设计全文：`../../research/2026-10-20-arc-v0-design.md`（修订版）
- 外部审核：`../../research/2026-10-20-arc-v0-review-and-competitor-analysis.md`（第一轮）；第二轮复审结论记在设计稿"修订记录"
- 对手调查：`../../research/2026-10-20-arc-competitors.md`
- 产品设计（三个界面、plan 层、全自动、app 即 runner）：`../../research/2026-10-22-arc-app-product-design.md`；它对 v0 设计稿的改动列在该文"对 v0 设计稿的改动"一节

## 决定

- Claude 是 lead：分析需求、拆 item、经 arc 的 MCP 工具写入 backlog、看判定、提交 rework brief；
  **批准（pass/fail）不在 MCP 工具里**，v0 这是流程保证不是安全边界（同一 OS 用户）；
  merge 由人执行。
- arc 是确定性程序，不是模型：文件 backlog 状态变更的**唯一写者**（`todo/doing/review/done/failed`
  目录为粗状态，frontmatter `phase` 为细状态），`state.lock` 短事务 + `state_version`，`runner.lock` 只放一个调度循环，不依赖 git 分支，不长驻
  （`arc run` 推进到没有立即可做的事就退；human、CI、退避等待不堵它）。
- 三个身份：`item_id`；`session_id = item_id`（**一个 work item 一个 session**，返工 `--resume` 回原
  host，不可恢复进 `failed`，人重新 add）；`attempt_id` 每次执行一个，旧 attempt 的结果被拒。
- worker 是一次性 `radian exec -p --profile workspace --yes --output jsonl`，工作区可写、无交互入口，工
  作区是 `.radian-worktrees/<item>` 的 linked worktree，单机 v0 共享一个 clone（跨 item 改 ref 的
  风险由包装器在 push 前检测：head 是 base 后代、branch 与 HEAD 一致、无 dirty，不成立即 failed 不 push，
  远端 ref 为真相）；host 侧的
  `radian work` 包装器在模型退出后核 commit/dirty/sha，**只在 `completed` 且产物有效时** push（git 置空
  hooksPath），写独立的 `AttemptFinished`；模型不持有 push 凭据。
- 交付核验由 arc 机器做（base/head sha、dirty、远端 ref、有效改动），不信 `Result.outcome`。
- 远端身份靠 host-local attempt record（token、pid + 启动时间）对账；包装器确认退出即 `terminated`，只有
  联系不上才 `reconciling`，不自动重派；失败分四类，只有基础设施与模型失败自动退避重试；预算
  `max_attempts/max_cost/max_wall_time` 到顶进 `todo.paused_budget`，`reset-budget` 后才继续，不与交付审批共用 phase。
- `done` 只等于已合并；gate 全过只到 `awaiting_human`。verdict 绑 sha，审批绑 sha + `verdict_set_digest`，
  分支动了或同 sha 重审都作废；pass 之后 Integration Gate 对 main 的合并候选验证，合并时 main 必须仍是那个 base。
- 闸门是可配置阶段序列：`commands` / `mutation` / `review` / `ci` / `human`，brief 只能加严；命令来源可
  信。只有固定 profile，无条件表达式、无 DAG；通用缺省 `commands → human → integration`；**radian 仓库 profile 加
  `mutation:required`**，复用仓库现有脚本。
- v0 推（ssh + stdin），v1 拉（`radian work --pull`，需要 `arc serve`）；`radian work` 两种都接。
- v1 要 desktop / iOS / web app：v0 冻结 identity、state、attempt、verification sha 四组字段并写
  `schema_version`；`run` 与 `serve` 共用状态机；UI 读 arc 存的 jsonl 流，不解析 attach。

## 待决

- brief 里 `mutations:` 的声明格式。
- host-local attempt record 的字段冻结。
- **第一个 Spike**：linked worktree 在 `workspace` 沙箱下跨 item 改 ref 的可行性与包装器检测覆盖（独立 clone 作对照）；同一 OS 用户下沙箱是否遮住
  push 凭据。jsonl 流在模型长思考时是否持续有行。
- 落地时用保留的 `serve` 还是新名 `arc`（改 ADR 0032）。

## 验收（落地时）

- 一个真实需求从 Claude 拆分到 `done/`，全程没有 herdr；每个 item 恰好一个 session 文件和一个 worktree；返工后
  `Result.turns` 递增而 session id 不变，attempt 目录递增；失败后新建的 item 带 `supersedes`。
- 一个 worker 在沙箱里改坏另一 item 的 ref 时，该 item 的 attempt 被包装器判 failed 且不 push；非 `completed` 的 attempt 没有 push；第二个 `arc run` 拿不到
  `runner.lock` 立刻退出；预算耗尽的 item 不能被 `pass`；同 sha 重审后旧 pass 无效；A 合入后 B 在 A+B 候选上复验，
  合并时 base 变了不合。
- `commands` 与 `mutation` 从不同树、**不同时**；`human` 停下后 `pass <item> <sha>`/`fail` 推进状态，
  sha 不符被拒。
- 审核报告第 10 节的故障注入全部通过：两个 `arc run` 并发只一个能 claim；ssh 断而远端仍跑时不重派；
  同 pid 复用不被认成旧 attempt；exit 0 但 dirty 或无产出不过；gate 验过 sha A 后分支推到 B 旧 verdict
  作废；A 合入后 B 复验；预算耗尽停下；等人时 `run` 能退且下次恢复。
- 落地同分支：新 ADR、改 0032/0043（docs-only commit）、删本文件。

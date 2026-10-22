# arc 对手调查（2026-10-20）

来源以官网/README/SPEC 为主；二手博客的说法标"二手"。未核实项标"未验证"。

## 四类产品

| 类 | 代表 | 做什么 | 和我们的关系 |
|---|---|---|---|
| 终端运行时 / 多路复用 | **herdr**、claude-squad、cmux、muxel | 保住 pane，知道 agent 是 working/blocked/done，API 做 read/send/wait/split/attach | herdr 自己的定位："坐在 Conductor/Emdash/Superset 下面"。我们要退出的就是这层 |
| 桌面管理器 | **Orca（onorca.dev，Stably AI，MIT）**、Conductor、Emdash、Superset、Nimbalyst（Crystal 继任）、Vibe Kanban（休眠）、OrchestratorInc | 每任务一个 worktree，diff 评审 UI，GitHub/Linear 集成，本地或 ssh 远端；27 种 agent CLI | 人盯着的交互式会话，长驻；无自动验证（Arc 的 review 是人工批注发回 agent） |
| 自治任务运行器 | **OpenAI Symphony**（spec + Elixir 参考实现）、**sortie**（单 Go 二进制 + SQLite）、Contrabass、cyrus、Machinist、Orbi、SuperPlane、autonomousagentai/agent-orchestrator、Solenta（未验证） | 以 issue tracker 为控制面：轮询、按 issue 开隔离工作区、起一次性 agent、产出 PR，人审 | **我们的设计属于这一类** |
| 群体 / 组织 | **Gas Town + Beads**（Yegge）、Paperclip | 持久身份 + 一次性会话；心跳领票；预算、审批、组织架构 | 规模 20–30 agent，复杂度远超我们需要 |

## 最接近我们设计的三个

### Symphony（SPEC.md 核实）
- claim 状态只有编排器改：Unclaimed / Claimed / Running / RetryQueued / Released；tracker 状态另算。
- 每次尝试一个 worker，worker 在该 issue 的工作区起一个新 app-server 会话；一个 worker 内最多 `max_turns` 轮，首轮给完整 brief，后续轮只给"继续"提示；session_id = `<thread>-<turn>`。
- 工作区按 issue 确定性命名，成功后**不删**，只在 issue 终态时清理；`after_create / before_run / after_run / before_remove` 钩子。
- 并发缺省 10；失败指数退避 10s 起，上限 5min；**stall 超时按"无输出"算**，不按总时长。
- 重启不保留任何内存状态：启动扫终态工作区、拉活跃 issue、重新派发。
- brief 是 `WORKFLOW.md` 模板按 issue 渲染，渲染失败只让这次尝试失败；凭据不传给子进程。
- 验证不在 spec 里：CI/测试/PR 归 workflow 提示词和 agent 工具。
- OpenAI 声明不作为产品维护。

### sortie（仓库与 docs 核实，2026-10-20 二次核查）
- 公开仓库 github.com/sortie-ai/sortie，Apache-2.0，3,212 个 commit；单 Go 二进制，SQLite 存重试队列、会话元数据、运行历史；agent 走 stdio 适配器（Claude Code、Copilot、OpenCode、Codex、Kiro、Gemini）；GitHub Issues / Jira。pkg.go.dev 见 v1.19.0（2026-08-11）；外部审核称有 v1.28.1 模块记录，本文未核到（GitHub Releases 为空）。
- Stage Chains：固定阶段间切换 prompt/agent/模型，每阶段新 session、工作区延续；线性链，不是 DAG。
- Self-Review：worker 退出前跑 verification commands、生成 diff、agent 给 verdict；docs 原话 `reviewer` 只接受 `"same"`，即**同 session**，不是独立 reviewer。CI 失败与 review 评论有限次回送，带 escalation。
- 同一套循环：轮询、隔离工作区、渲染提示、stall 检测、超时、退避重试、与 tracker 对账、清理。
- **是长驻服务**（持续轮询 tracker、health probe、Prometheus 指标）。

### Gas Town / Beads（README 核实 + 二手）
- 会话一次性、身份持久（存 git）。Polecat 取一个 bead → worktree → 提 MR → 回收。Refinery 是 Bors 式合并队列：批量合并、跑一次测试、失败二分。
- **Beads 现以嵌入式 Dolt（单写者）为源数据，server 模式才多写者；JSONL 只是导出；hash id 防合并碰撞；`bd update --claim` 原子领取。** 仓库现在在 gastownhall/beads。README 没有说明离开 JSONL-in-git 的原因，所以只能得出：多写者在各自分支改同一份任务状态、靠 git 合并解决并发，是高复杂度设计；不能推出"文件进 git 绝对不可行"。 二手报告的问题：并行 agent rebase 同一分支各自解决冲突导致历史分叉；Claude Code 收到任务后礼貌地等人输入（"politeness deadlock"），只能 tmux send-keys 推一下；约 $100/h。

### 其它
- Paperclip：Postgres 存票，单 assignee + 执行锁，心跳唤醒，预算警告与硬停、审批阶段；Node，MIT。
- **OrchestratorInc**（docs.orchestrator.inc/guides/review-loop）：有专用 reviewer agent，"is not the worker agent"，不拥有 worker 分支，按 PR 当前 head 记录 verdict，同一 head 不重复 review，required 评论可自动回送 worker。docs 没说 reviewer 是否自己跑测试，也没说沙箱。
- autonomousagentai/agent-orchestrator：Markdown 文件 backlog（`### slug:` 标题），每轮一个任务起**新会话**，"shipper" 负责 PR/目录投递；systemd 长驻；4 个 commit，极早期。
- amux：SQLite compare-and-swap 领取，输家 409。
- Orca（onorca.dev）的 worker 生命周期官网没说；"NEEDLE"（列表里"共享 bead 队列原子领取"）搜不到仓库。

## 对我们设计的印证与修正

印证：
1. 多写者经 git 分支合并共享任务状态是高复杂度设计（Beads 现以单写者 Dolt 为源）。单写者目录避开了这个问题，不用引入数据库。
2. "只有编排器改调度状态"、"重启不留内存状态"、"工作区活到终态"：Symphony 原话，和我们的 arc 一致。
3. 一次性 worker + resume 返工：Symphony 的 thread 复用、Gas Town 的一次性会话都这么做。
4. headless 无输入口避开了 Gas Town 的 politeness deadlock。

修正（设计稿要加）：
5. **stall 超时按无输出计**（Symphony），不是总时长；radian 的 `--output jsonl` 流正好能算。
6. **退避重试**与并发上限：设计稿没写。
7. 独立 reviewer 不是空白：OrchestratorInc 有专用 reviewer agent；Sortie 的 self-review 是同 session。**没人做的是：绑定固定 sha 的隔离 commands gate 加变异检查、verdict 随 sha 失效、合并前集成复验。** 差异点要具体到验证契约，不是"多一个 agent 做 review"。
8. 没人做"不长驻"：Symphony、sortie、Paperclip 全是服务。`arc run` 排空即退是取舍不是必然优势：部署简单，代价是不主动轮询、不收远程推送、重试靠定时器。

## 要不要自己做

- 桌面管理器那一类不做：和"worker 只读、人只观察"相反。
- 任务运行器那一类有现成的：若 backlog 用 GitHub Issues，sortie 或一个 Symphony 实现加一个 `radian exec --output jsonl` 的 stdio 适配器，今天就能跑，代价是长驻、需要 tracker、没有固定 sha 的独立 gate。外部审核建议与 Sortie 做双路径实验；决定不做整轮 A/B，只读其 adapter 接口做参照。
- 第二轮审核（同日）对竞品没有新纠正；它建议参考 Sortie 的 adapter、Stage Chains 与 CI feedback 实现，避免重复踩已解决的问题，owner 接受。
- 建议：自己做小的 arc，**抄 Symphony 的状态机和超时/退避语义**（其 SSH 附录只要求区分主机失联与任务失败，没有定义远端进程身份与 push，这两处要自己做），保留差异点：文件单写者、不长驻、固定 sha 的独立 gate 与 verdict 失效规则。先做仓库外原型。

## 来源
- https://www.onorca.dev/
- https://herdr.dev/compare/
- https://raw.githubusercontent.com/openai/symphony/main/SPEC.md
- https://github.com/openai/symphony
- https://github.com/gastownhall/beads（原 steveyegge/beads）
- https://www.augusteo.com/blog/inside-gas-town/ （二手）
- https://github.com/paperclipai/paperclip
- https://github.com/autonomousagentai/agent-orchestrator
- https://github.com/andyrewlee/awesome-agent-orchestrators
- https://github.com/sortie-ai/sortie 、https://docs.sortie-ai.com/guides/configure-self-review/ 、https://pkg.go.dev/github.com/sortie-ai/sortie
- https://docs.orchestrator.inc/guides/review-loop/
- 二手综述：https://www.augmentcode.com/tools/open-source-agent-orchestrators 、https://nimbalyst.com/blog/best-multi-agent-coding-tools-2026/ 、https://betterstack.com/community/guides/ai/openai-symphony/ 、https://amux.io/demos/atomic-task-claiming/

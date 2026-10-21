# 深挖 9：长任务与记忆 —— Radian 与五个对手

日期：2026-10-18 · 代码版本：Radian `356df1e`；Codex `8c07694`；opencode `63c1a27`（v2 core）；pi-mono `482c138`；omp 18.12.1 npm 包；Claude Code 只看官方文档（每条注明）。
范围：跨很多轮、跨会话的那一部分。压缩算法、缓存、摘要结构、压缩后补水见 [3-context.md](3-context.md)；空停止、截断续写、doom-loop、步数到顶收尾见 [1-core-loop.md](1-core-loop.md)。本文只在必要时引用，不重复。

出处前缀：`rad/` = `<repo>`；`codex/` = `comp/codex/codex-rs`；`oc/` = `comp/opencode/packages/core/src`；`pi/` = `comp/pi-mono/packages/coding-agent`；`omp/` = `comp/npm-_oh-my-pi_pi-coding-agent/package/src`（`comp/` 在 scratchpad 下）。CC = Claude Code。

---

## 1. 一句话结论

Radian 在“中断后不坏”这件事上做得最扎实：journal 可以确定性重放，崩溃时没跑完的工具调用会补上结果并写清楚原因，后台 job 成了孤儿也会提醒，fork 只在轮间边界做，记忆的治理也最严。但长任务真正慢在**行为**上，Radian 在这方面几乎没有机制。dogfood9/10/11 三次都是“先核实一切再动笔”：D24 一轮 445 次调用、10 次压缩，没有一次编辑；D25 前 239 次调用没有编辑；D26 前 45 次调用（约 23 分钟）没有编辑。三次唯一起作用的是 owner 发的一条新消息。这三次之前，ADR 0018 已经改过摘要（修订 5、6）。五家对手里有四家用 **持久化的目标或 todo，加上宿主在轮末自动续跑或提醒** 来处理这个问题：Codex `/goal`、CC `/goal`、omp `goal`/`todo`、opencode 的崩溃自动续跑。Radian 收益最高的改动按顺序是：①“迟迟不编辑”看门狗（宿主替 owner 发那条 steer）；②持久化的 todo/plan，并且确定性地带过压缩；③`/goal` 续跑，加完成审计和预算；④resume 一键续跑；⑤记忆写入指引和会话末尾的经验捕获。

---

## 2. 对比表

| 维度 | Radian | pi | Codex | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| todo / 计划工具 | **没有**（ADR 0043 删了 `.radian/progress/`，`rad/docs/adr/0036-…md:57`） | 设计上不内建，`todo.ts` 是扩展示例（`pi/README.md:19`，`pi/docs/extensions.md:375`） | `update_plan`：steps 加 pending/in_progress/completed，最多一个 in_progress；返回只有 `"Plan updated"`，计划只活在历史里（`codex/core/src/tools/handlers/plan_spec.rs:7-56`，`plan.rs`） | `TodoWrite`（content/status/activeForm）、`TaskCreate/Update/List`（可写依赖 `addBlocks`）（`sdk-tools.d.ts:1098,2802-2868`） | `todo`（分阶段）：默认开；**结束时还有未完成项就提醒，最多 3 次**；连续 12 次修改都没更新 todo 时，中途提醒，每轮最多 2 次；压缩后重新提醒（`omp/tools/settings.ts:348-393`，`omp/session/todo-tracker.ts:17-18,218-280,325-335`） | v2 core **删了 `todowrite`**（`oc/database/v1-migration.bun.ts:901`） |
| 计划模式 | 没有（ADR 0043 删了 mode） | 不内建（`pi/README.md:19`） | 有 collaboration-mode 模板（未细查） | 只读研究后写计划，批准时选择接下来的权限模式；可选“批准并清空规划上下文”（`showClearContextOnPlanAccept`）（[permission-modes](https://code.claude.com/docs/en/permission-modes)） | `plan-mode/`：批准的计划存成文件，之后作为“plan reference”交给主 agent 和 subagent（`omp/plan-mode/plan-handoff.ts:13-21`，`plan-autosave.ts`） | `plan` agent：只允许改 `~/.opencode/plan/*`，每次请求都校正提醒，因为压缩或 revert 会把提醒剥掉（`oc/plugin/plan.ts:13-21,37-41,56-60`） |
| 目标 / 自动续跑 | 没有。用户原话由 Core 确定性地带过压缩（ADR 0018），但轮结束就停 | 没有 | **`/goal`，默认开**（`codex/features/src/lib.rs:1936-1940`）。目标存进 state DB；空闲时自动起续跑轮（`ext/goal/src/runtime.rs:436-535`）；续跑提示里有 No-progress check、Completion audit，被同一个问题挡住连续 3 轮才允许报 blocked（`ext/goal/templates/goals/continuation.md`）；可设 token 预算，到预算注入 wrap-up 提示（`budget_limit.md`）；轮出错或空回复时停止目标（`ext/goal/src/extension.rs:314-447`）；目标 ≤4 000 字符（`protocol/src/protocol.rs:4216`） | `/goal <条件>`：每轮结束由**小模型**判 met / not yet / impossible，没满足就继续；连续几轮不用工具就停；后台任务在跑时跳过判定，30 分钟后 check-in，指数退避；resume 时恢复目标（[goal](https://code.claude.com/docs/en/goal)） | `goal` 默认开，交互模式自动续跑（`omp/goals/settings.ts:3-6,27-32`），续跑提示含完成审计（`omp/prompts/goals/goal-continuation.md`） | 没有目标；但有**崩溃后自动续跑**（见下一行） |
| 崩溃 / 中断后恢复 | `--resume`：持锁解析一次，没配对的调用补 `outcome_unknown_after_crash`（“可能已经执行，结果未知，不要假设没跑”）或 `not_started_after_crash`（`rad/crates/agent-core/src/journal.rs:398-403`）；孤儿后台 job 补一条消息（`rad/crates/radian-host/src/job_notify.rs:198-236`）；**不自动续跑**（grep 无结果），要用户输入 | 打开会话文件继续 | resume 时恢复 active goal，下次空闲时自动续跑（`ext/goal/src/runtime.rs:412-434`） | 中断的工具标为被截断，提示先确认是否生效；**不自动起轮**；后台 Bash 不恢复，只留一条“未完成”的记录（[sessions](https://code.claude.com/docs/en/sessions)） | 未细查 | **服务重启后自动续跑孤儿轮**，注入“The server restarted… Continue from where you left off without repeating completed work.”；每轮最多续 10 次，防止崩溃循环（`oc/session/execution/restart.ts:15-16,33`）；后台 shell 报 cancelled，subagent job 会恢复（`restart.ts:101-185`；`oc/job.ts:10-30`） |
| 文件回滚 / checkpoint | **没有**（ADR 0022 写明 `Rewound` 未定） | 没有（tree 只分叉对话） | ghost-commit `undo` 已**移除**（`codex/features/src/lib.rs:1058-1062`，Stage::Removed）；Esc Esc 回到某条 prompt 重新编辑，只回退对话（`codex/tui/src/app_backtrack.rs:1-14`） | 每条起轮的 prompt 一个 checkpoint，保留最近 100 个，约 30 天；`/rewind` 可以回退代码、对话或两者；**Bash 改的文件不跟踪**（[checkpointing](https://code.claude.com/docs/en/checkpointing)） | `checkpoint`/`rewind` 只回退**上下文**，不回退文件（默认关，`omp/tools/settings.ts:688-690`） | 每个 step 前后在**影子 git 仓库**里取工作区快照（`oc/snapshot.ts:95-96`；`oc/session/runner/step.ts:76,237`）；revert 分 stage/clear/commit 三步，可以撤销（`oc/session/revert.ts:21-114`）；**连 shell 的改动也覆盖**（推断：快照的是整个工作区树） |
| fork / 分支 | 只在轮间边界 fork，物化有效视图，先写子再记父（ADR 0022）；`radian exec --fork`、TUI `/fork` | **会话树**：每条 entry 有 parentId，`/tree` 切换分支；离开一个分支时自动生成 **branch summary**（Goal/Progress/…）带到新位置（`pi/core/session-manager.ts:977-985`；`compaction/branch-summarization.ts:1-6,258`） | `/fork`，Esc Esc 回溯 | `/branch`、`--fork-session`；进程内 branch 保留会话级授权，后台任务继续跑（[sessions](https://code.claude.com/docs/en/sessions)） | 继承 pi 的树 | `Session.fork`（`oc/session.ts:99,132`） |
| 跨会话记忆：谁写、什么时候写 | 只有显式写入：`memory_save`/`memory_revise`，**默认 ask**（`rad/crates/radian-permissions/src/profile.rs:275-276`）；M1 不做自动抽取（ADR 0021:127）；工具描述只讲 scope，**不讲什么时候该存**（`rad/crates/radian-host/src/memory/tools.rs:476-487`） | 没有 | 离线两阶段抽取，**默认关**（`features/src/lib.rs:1302-1306`）：启动时挑空闲 ≥6 h、≤10 天的会话，每次最多 2 个（`config/src/types.rs:55-57`），模型抽出 raw memory，再由一个合并子 agent 写 `MEMORY.md`、`memory_summary.md`、`skills/`（`memories/README.md`） | **auto memory 默认开**：模型在会话中自己写，分 user/feedback/project/reference 四类，跳过能从代码推出的内容（[memory](https://code.claude.com/docs/en/memory)） | `memories`（照 Codex 的管线，默认关，`omp/memories/settings.ts:11-14`）；`autolearn`（默认关）：一轮里工具调用 ≥5 次后插一个**捕获轮**，把可复用的流程写成 managed skill，事实交给 `learn`（`omp/autolearn/controller.ts:1-13`，`settings.ts:9-13,38`） | 没有 |
| 记忆格式与注入量 | 追加写 JSONL，Markdown 只是视图；固定段 ≤2 000 tok（Global 500 / Project 1 500 软配额），按会话固定；后续轮的新命中 ≤500 tok 作为消息追加（ADR 0021 D6、修订 2） | — | `memory_summary.md` 截到 **2 500 tok** 注入，其余用 search/read 渐进读取（`ext/memories/src/lib.rs:16`；`templates/memories/read_path.md`，6.6 KB） | `MEMORY.md` 索引的前 200 行或前 25 KB；主题文件按需读；接近上限时提醒模型精简（文档） | 摘要注入上限 5 000 tok（`omp/memories/settings.ts:90-92`） | — |
| 过期 | v1 没有 TTL（ADR 0021:76）；只有手动 Archive/Forget/Purge | — | 30 天没用过的记忆不再参与合并（`max_unused_days`，`types.rs:60`）；合并时按使用次数排序 | 不过期；写入时记 `modified` 时间戳；记忆文件不受 30 天保留清理影响（文档） | 会话年龄 ≤30 天才抽取（`settings.ts:23`） | — |
| 后台任务与通知 | 运行中注入、空闲时唤醒，连续唤醒最多 3 次（`job_notify.rs:35`）；最多 4 个 job；前台跑过 120 s 自动转后台（ADR 0037） | — | — | 后台 Bash/subagent 完成后作为新一轮送达；goal 模式下延后判定并定期 check-in | 有（未细查） | job 注册表，完成通知持久化，**重启后补送**（`oc/job.ts:170-200,441-449`） |
| 预算 / 步数 | 宿主默认 1 000 次模型调用，用完 `BudgetExceeded`（`rad/crates/radian-host/src/model.rs:36`）；没有 token 预算 | 无 | goal 的 token 预算，到预算时注入 wrap-up 提示 | `--max-turns`、`--max-budget-usd`；goal 的条件里可以写“20 轮后停” | goal 的 token 预算 | 最后一步 `toolChoice:none`（见 1-core-loop） |

---

## 3. 各家的关键做法（只列和“少打转、少重做、快续上”有关的）

### Codex
- **`/goal` 是 Codex 处理长任务的主机制**（stable，默认开）。用户给出 objective，存进 state DB。每轮结束时如果目标还 active，宿主在空闲时用续跑提示起下一轮（`ext/goal/src/runtime.rs:436-535`）。提示要求模型先判断上一轮属于哪一类：progress、verified wait 还是 no progress；“复述状态、只写计划不执行都算没有进展”。完成之前必须按需求逐条找“权威证据”，测试通过只有在确认覆盖了该需求时才算证据。被同一个问题挡住连续 3 轮才能报 `blocked`。还明确禁止“把成功重新定义成一个更小、更容易通过测试的子集”（`continuation.md`）。
- **防止空转烧 token**：轮出错（包括压缩出错）会停止目标，注释原话是“to prevent automatic continuation from looping and consuming tokens, as can happen with compaction errors”（`extension.rs:423-447`）；自动续跑如果得到空回复，目标也停（`accounting.rs:228-240`）。
- 目标经过压缩时作为单独的 user-goal 片段保留，超长的 objective 整段省略，不做截断（`core/src/context/user_goal.rs:1-3,14`）。
- `update_plan` 只是“让计划可见”，返回固定字符串，状态不在宿主侧（`plan.rs`）。goal 的续跑提示会在多步工作时要求模型更新 plan。
- 记忆管线（默认关）的抽取提示把这些列为值得记的东西：**仓库导览、先 grep 哪里、常见失败和已验证的修法、“快速确认成功或发现方向错了”的停止规则**（`memories/write/templates/memories/stage_one_system.md:132-136`），目标是“下次用更少的工具调用和推理 token 解决同类任务”（`:10`）。读取侧建议记忆查询控制在 4–6 步以内（`read_path.md`）。
- 文件 undo（ghost commit）已经移除（`features/src/lib.rs:1058-1062`）。

### Claude Code（全部来自文档）
- **`/goal` 由一个独立的小模型判定**：干活的模型不能自己宣布完成。判定器不调工具，只看对话里已有的证据，所以条件要写成“`npm test` exit 0”这种能在 transcript 里看到的形式。连续几轮不用工具就停下来交还用户。有后台任务时跳过判定，30 分钟后 check-in，之后间隔翻倍，最长是第一次间隔的 4 倍；空闲时的 check-in 最多 3 次。会出现“必须由你修”的错误（认证、额度、压缩清不掉的溢出、模型不可用）时目标直接清除；其他错误重试 3 次后暂停（[goal](https://code.claude.com/docs/en/goal)）。
- **checkpoint**：每条起轮的 prompt 一个，保留 100 个，约 30 天；`/rewind` 可以回退代码、对话或两者，也可以“从这里开始摘要”或“摘要到这里为止”；Bash 改的文件、subagent 改的文件和外部修改都不跟踪（[checkpointing](https://code.claude.com/docs/en/checkpointing)）。
- **plan mode**：批准时可以选“批准并清空规划上下文”，探索阶段读进来的东西不会带进实施阶段（[permission-modes](https://code.claude.com/docs/en/permission-modes)）。
- **auto memory**：默认开，由模型在会话中写，按 git 仓库分目录（worktree 共享），`MEMORY.md` 是一行一条的索引，只加载前 200 行或 25 KB；不记能从代码推出的内容（[memory](https://code.claude.com/docs/en/memory)）。
- resume 会恢复模型、权限模式和 active goal；中断的工具会提示“先确认它是否已经生效再重跑”；不会自动起轮（[sessions](https://code.claude.com/docs/en/sessions)）。

### opencode
- **崩溃或重启后自动续跑孤儿轮**，注入一句固定话术，要求不要重复已经完成的工作；每轮的续跑次数持久化，最多 10 次（`restart.ts:15-33`）。后台 shell 报 `cancelled`，但不会唤醒空闲的会话；subagent job 会接着跑，并把结果送回父会话（`restart.ts:101-185`）。
- **影子 git 快照**：每个 step 的开头和结尾各取一次工作区树，revert 可以按消息回退文件，而且可以撤销（`snapshot.ts:95-96`，`revert.ts`）。快照放在全局数据目录，不在项目里。
- plan agent 的提醒会被压缩剥掉，所以每次请求都重新检查一遍，补的时候加在尾部，保住缓存前缀（`plugin/plan.ts:56-60`）。这是“模式状态要在压缩后确定性恢复”的一个实例。

### pi
- 会话就是一棵树；切到别的分支时，被离开的分支会生成一份结构化的 branch summary 带过去。这样换方案时，模型知道上一条路试过什么、为什么放弃（`branch-summarization.ts:1-6,258`）。
- 刻意不内建 todo、plan mode 和 subagent，交给扩展去做（`README.md:19`）。

### omp
- **todo 加提醒**：结束时还有未完成项就提醒，最多 3 次；“正在等用户回答”时不提醒；连续 12 次修改都没碰 todo 时，中途提醒，每轮最多 2 次；压缩后补一次 eager 提醒（`todo-tracker.ts:17-18,218-280,325-335`）。
- **goal**（默认开，照 Codex）、**autolearn**（默认关）：工具调用 ≥5 次的一轮结束后，插一个合成的捕获轮，提示写明“这不是用户的回复……没有值得保留的就什么都不做，然后停”（`prompts/system/autolearn-nudge-autocontinue.md`）。
- **checkpoint/rewind 回退的是上下文**：探索之前打一个 checkpoint，探索完 rewind，只留下一份简短报告（`prompts/tools/checkpoint.md`）。专门用来解决“摸索把窗口填满”。
- **autoresearch**：实验循环，有持久化的 notes，每轮都注入 system；`log_experiment` 在 keep 时自动 commit，在 discard 时自动回退工作区；resume 时提示“先补完上次没记录的那次运行”（`autoresearch/prompt.md`，`resume-message.md`）。

---

## 4. Radian 的优势

1. **中断语义最清楚。** 没配对的调用分成“可能已经执行”和“没开始”两种，写进工具结果（`journal.rs:398-403`）。孤儿后台 job 补一条消息，并说明没有重跑（`job_notify.rs:229-236`）。fork 只在轮间边界做，崩溃顺序是先写子、再记父（ADR 0022）。opencode 的自动续跑要靠 DB claim；Radian 的 journal 加 `recover` 足够支撑同样的功能（§6.4），不用改存储。
2. **目标原话确定性地带过压缩**（ADR 0018），摘要里有 `Verified facts (do not re-check)` 和 `Current work`/`Next step`（修订 5）。五家里只有 Codex 的 goal 片段有类似的确定性保留。
3. **记忆治理最严。** 来源不可伪造，用户记录优先，revision 比较交换，Forget 之后不会复活，Purge 有崩溃安全的写法，沙箱读不到记忆存储（ADR 0021 不变量 1–6）。注入段按会话固定，不破坏缓存（修订 2）。CC 的 auto memory 只是普通 Markdown 文件，模型想改就改。
4. **后台任务的通知规则完整**：运行中注入、空闲时唤醒，连续唤醒最多 3 次，前台命令自动转后台，孤儿 job 有提醒（ADR 0037）。在 CC 之外，这是做得最细的。
5. **有 `explore` subagent**（只读、结论 ≤4 000 字符，`rad/crates/radian-host/src/subagent.rs:60`）。它已经能像 omp 的 checkpoint/rewind 那样把摸索隔离在主上下文之外，只是没人引导模型去用（§5.6）。

## 5. Radian 的劣势和缺口

1. **“迟迟不编辑”没有任何机制管，这是 dogfood 里最贵的问题。**

   | 会话 | 现象 | 唯一起作用的干预 |
   |---|---|---|
   | dogfood9 D24 | 恢复后的一轮：445 次调用、10 次压缩、0 次 patch/write，最后 `TurnFailed` | 无 |
   | dogfood10 D25 | 前 239 次调用、4 次压缩没有编辑；每份摘要的 `Next step` 都写着“按规则 4 把 X 节写进指南” | 第 239 次调用时 owner steer，第 244 次出现第一个 patch，之后按节落盘 |
   | dogfood11 D26 | 小任务，前 45 次调用（约 23 分钟）没有编辑 | owner steer 后 6 分钟做完 |

   出处：`rad/docs/findings/2026-10-12-radian-tui-dogfooding.md:35-37`。findings 的结论原话是：“The ‘verify everything before writing’ habit is unchanged by any mechanism tried so far … only a new user message moves it.” 摘要写对了（修订 5、6）并没有改变行为（D25）。
2. **轮结束就停，不会自己接着干，也不会自己停。** D25 做完清理（第 459 次调用）后没有结束这一轮，反而开始读别的分支，直到第二条 steer。D26 中间 owner 重启过一次，靠发“continue”接上。Radian 没有目标状态，也没有完成判定，只有提示词里的 Stopping 一节（`rad/crates/radian-host/prompts/default.md:36-38`）。
3. **没有持久化的计划。** 进度只靠模型写的 `Progress`/`Pending` 小节带过压缩，每次滚动压缩都会再转述一遍，D24 的第一个归因就是这个。CC、Codex、omp 都有 todo/plan 工具；omp 还在结束时和中途提醒，压缩后再补一次。
4. **resume 不续跑**：被打断的轮要人来说“continue”。opencode 会自动续跑并要求“不要重复已完成的工作”；Codex 和 CC 在有 active goal 时会恢复目标。
5. **没有文件回滚。** 模型想“试一下再退回”只能自己用 `git checkout`/`git stash`，人想撤销模型的改动也只能用 git。`Rewound` 还没决定（ADR 0022）。对效率的影响是间接的：不敢大胆试，或者回退时多花几轮（推断）。
6. **开局摸索没有人引导。** ADR 0036 的背景记录了 dogfood 2026-10-14 开头 28 分钟都在摸索项目（`rad/docs/adr/0036-…md:13`）。根因是当时 AGENTS.md 没被读到，现在已经修了，但“摸索把主上下文填满”这个问题本身还在（D25 里关于指南的工具输出约占全部工具输出的一半）。`explore` subagent 有了，提示词里却一个字也没提。
7. **记忆基本是空的（推断）。** `memory_save` 默认 ask，工具描述只说 scope 怎么选，没说**什么时候**该存（`memory/tools.rs:476-487`）；也没有任何自动抽取（ADR 0021:127）。CC 把“用户纠正过的、能节省用户按键的东西”当作首要的记忆来源（Codex 的抽取提示 `:115-123` 也是这样）。Radian 有存储，但缺了写入的那一侧。（本机读不到 `~/.radian/memory/`，实际记录数没有核实。）使用数据：测量用的三个会话一共 479 次工具调用，memory 工具一次都没有出现（`rad/docs/research/2026-10-15-tool-concurrency-measurement.md` 的合计表）。样本只有三个会话，但方向和上面的推断一致。`memory_search` 也很少有机会用：会话开始时已经自动注入了最多约 2 000 tok 的记忆段。
8. **没有 token 或费用预算**：只能按模型调用次数限制，到顶就是硬停。Codex、omp 的 goal 和 CC 的 `--max-budget-usd` 都能按 token 或美元收口。

---

## 6. 改进方案（按收益/成本排序）

### 6.1 “迟迟不编辑”看门狗（收益最高，成本低，需要新 ADR）
- **做什么**：宿主在每次模型调用之前（agent-core 已有的 `before_model_call` 钩子，记忆注入就在用它）统计当前轮“自最后一次 `patch`/`write` 以来的工具调用数和压缩次数”。超过阈值时，往 inbox 的 `inject` lane 放一条 System 来源的消息，例如：“你已经连续 N 次调用、M 次压缩没有改文件。如果任务要求改文件：停止核实，用 patch 写下你已经确认的部分，没把握的地方标出来留到最后。如果任务本来就不需要改文件，说明原因并继续。”阈值建议：调用数 40 **或** 1 次压缩，以先到者为准；每轮最多 2 次（照 omp 中途提醒的 `MAX_PER_CYCLE = 2`）。只读任务、explore subagent、`ask_user` 正在等待时不发。
- **为什么**：这是 Radian 自己 3 次 dogfood 里唯一验证有效的干预：D25 在 steer 之后 5 次调用就出现第一个 patch，D26 在 steer 之后 6 分钟做完。按 D26 估算，阈值 40 会在约第 40 次调用时触发，比 owner 的第 46 次稍早；按 D25 估算能省掉约 200 次调用、3–4 次压缩（推断）。omp 的中途提醒（12 次修改没更新 todo）和 Codex 的 No-progress check 是同一类思路，但它们针对的是“干了活不记账”，没有针对“只读不写”。Radian 这个失败模式是自己特有的，需要自己的触发条件。
- **成本**：宿主几十行，不改 agent-core，复用 ADR 0037 的 inject 路径（`job_notify.rs:7-10`）。要写一个新 ADR，或者在 0011 上加修订，定下“宿主发起的行为提醒”：来源是 System；按 ADR 0043 第 4 条规则 5，有非人输入时有效来源只降不升，权限不受影响；内容固定；有次数上限。**未核实**：System 来源的提醒是否和 owner 的 User 消息一样有效。D25 的摘要（也是 System 来源）就没起作用，所以要实测。
- **验证**：用 D26 的任务和基线 commit（findings 里写了 §8 的范围）跑 3 次开和 3 次关，比第一次编辑出现在第几次调用、总墙钟、总 token。变异：阈值失效、计数没在 patch 时归零、每轮上限失效，三种情况都要有测试失败。

### 6.2 持久化的 todo，确定性地带过压缩（收益高，成本中，需要新 ADR，碰到 ADR 0018、0043）
- **做什么**：一个 `todo` 工具，参数照 Codex/CC 的最小形状：`items[{text, status: pending|in_progress|done}]`，整表替换，最多一个 in_progress。宿主把每次更新写成 durable 运行时事件（例如 `TodoUpdated`，`critical:false`）。**压缩时由 Core/宿主把当前 todo 表原样拼进摘要消息**，做法和修订 6 带用户原话一样，不经过模型，然后从 `Progress` 小节里删掉重复的部分。结束时还有 pending 或 in_progress 项、并且没有在等用户回答时，提醒一次（omp 最多 3 次，`todo-tracker.ts:229-280`；Radian 建议 1–2 次）。
- **为什么**：D24 的第一个归因就是“进度只以模型转述存活，每次滚动再转述一遍”。修订 6 已经让原话确定性存活，todo 是把同样的待遇给进度。结束时的提醒同时管 §5.2 的“没做完就停”。todo 本身不能解决 §6.1 的问题：D25 的 `Next step` 也写得很具体，模型照样不动。所以两者要一起做。
- **成本**：一个工具定义约 0.5 KB（每次请求都要带，3-context §6.7 已经指出前缀偏重）；一个 runtime 事件，加 fixture 和 `check-fixtures.sh`；摘要拼接在 ADR 0018 的修订 6 旁边再加一个修订。ADR 0043 第 3 条把工具数定为 17，加一个就要修订那张表。
- **验证**：回放 D24/D25 那样的长会话，看压缩前后 todo 表逐字一致；统计“压缩后前 10 次调用里重新读同一文件”的次数。变异：拼接漏掉 → 测试失败；结束提醒在等用户回答时也发了 → 测试失败。

### 6.3 `/goal`：目标续跑、完成审计、token 预算（收益高，成本中到高，需要新 ADR）
- **做什么**：`/goal <条件>`（TUI）和 `radian exec --goal`。目标写进 journal（durable，resume 时恢复，照 CC 和 Codex）。轮结束时目标还 active，宿主就用续跑提示起下一轮（走 ADR 0011 的唤醒 lane），提示内容照 Codex `continuation.md` 的 No-progress check、Completion audit 和“不要把成功缩小”。**完成判定**有两种做法：照 CC 用一个不调工具的小模型判（不能让干活的模型自己判，D25 就证明了它不知道什么时候停），或者照 Codex 让模型调 `update_goal(complete)`，再由宿主核对。建议先照 CC，评估器的 token 很少。防护照抄：轮出错、空回复或连续 3 轮没有工具调用就停（Codex `extension.rs:423-447`；CC 文档）；后台 job 在跑时延后判定（Radian 已有 job 注册表）；可选 token 预算，到 90% 注入 wrap-up 提示（Codex `budget_limit.md`）。
- **为什么**：解决“没做完就停，要人说 continue”（D26 重启后）和“做完了不停”（D25）。CC 文档把 `/goal` 和 auto 模式说成互补：auto 去掉每个工具的确认，`/goal` 去掉每一轮的确认。Radian 的 dogfood 一律 `full --yes`，正好缺后一半。
- **成本**：中到高。要有一个小模型的路由（ADR 0026，3-context 里也提过压缩用小模型），一个 durable 目标事件，一个续跑提示，还有和 ADR 0037 最多 3 次连续唤醒的关系（goal 的续跑轮要单独计数，否则会被这个上限截住）。ADR 0017（模型或 hook 出错就结束这一轮）不变，goal 只是在轮出错时停下。
- **验证**：评测集里“条件可以从 transcript 判定”的任务（`cargo test -p X` 通过、某个文件存在）跑开和关，看人工介入次数、成功率和总 token。变异：评估器一律回答 not yet 时，3 轮无进展的防护必须停下。

### 6.4 resume 一键续跑（收益中，成本低，需要修订 ADR 0011 或 0022 的实施说明）
- **做什么**：resume 时，如果最后一轮不是 `completed` 结束的（cancelled 或崩溃），TUI 显示一行“上一轮被中断，按 c 继续”，`radian exec --resume <id> --continue` 也一样。继续时发一条固定的 System 消息，内容照 opencode：“会话在你工作时被中断。先检查标为 outcome_unknown 的调用是否已经生效，然后从中断处继续，不要重做已完成的工作。”**不要**像 opencode 那样完全自动续跑：Radian 不是常驻服务，自动续跑只在有 `/goal` 时才合理（并入 6.3）。
- **为什么**：省掉一次手打，也把“不要重做”这句写成固定话术。现在的 `outcome_unknown_after_crash` 只说了“别假设没跑”，没说“别重做”。
- **成本**：低。终止原因已经在 journal 里（`TurnFinished` 带 cancelled/requested）。
- **验证**：pty 测试：中断之后 resume 显示提示，按 c 发出那条消息；不是中断结束的会话不显示提示。

### 6.5 记忆写入指引和会话末尾的捕获（收益中，成本低到中，碰到 ADR 0021）
- **做什么**：(a) 在 `memory_save` 的描述里加上“什么时候存”，照 CC 的四类和 Codex 抽取提示的重点：用户纠正过的偏好、项目里不能从代码推出的事实或决定、验证过的“这个仓库怎么跑测试或怎么查”的捷径；明确不存能从代码或 AGENTS.md 推出的东西。(b) 允许按项目把 `memory_save` 设成 allow（ADR 0021 D5 的默认 ask 不变，但 `--yes` 之外要有一个不用每次问的办法），否则这个工具实际上用不起来。(c) 第二步再做：照 omp autolearn，在一轮工具调用超过阈值、并且以 `completed` 结束时，**可选地**插一个捕获轮。ADR 0021 D9 已经给自动抽取留了位置：只能读已提交的会话事实。 (d) 精简工具面：`memory_save` 和 `memory_revise` 合并成一个工具，传 `id` 就是修改（2-tools P4 第 4 条）；`memory_search` 只在记录多到注入段装不下时才挂载。这样模型看到的 memory 工具从 3 个降到 1–2 个。`memory_get` 不用再处理：它是 ADR 0043 留的一版别名，宿主已经从每次请求里剥掉，模型看不到（`rad/crates/radian-host/src/model.rs:187-200`）。
- **为什么**：Radian 的记忆存储已经做好，缺的是写入。Codex 写明记忆的目标是“下次用更少的工具调用解决同类任务”，抽取提示把“仓库导览”和“先 grep 哪里”列为重点（`stage_one_system.md:10,132-133`），这正对着“开头 28 分钟摸索”。不过 CC 认为不该记能从代码推出的内容。Radian 有 AGENTS.md，所以 (a) 的重点应该是“用户纠正过的”和“验证过的捷径”。
- **成本**：(d) 合并工具要写 ADR 0043 修订（第 3 条定了工具表），按需挂载 `memory_search` 会让工具列表在会话中途变化，要和 2-tools P8 的缓存问题一起设计；(a) 只改描述，加上 `the_save_description_states_when_global_is_right` 那样的描述测试；(b) 是权限配置；(c) 要一个 ADR 0021 修订，是 M3 范围里最小的一片。
- **验证**：A/B 同一个仓库连续两次相关任务，比第二次第一次编辑之前的调用数。这需要先有评测集（总报告第 0 步）。

### 6.6 提示词里引导用 `explore` 处理开局摸索（收益中，成本很低，碰到 ADR 0036）
- **做什么**：在 `default.md` 的 “Reading before you change anything” 下加一条：“要大范围了解陌生代码时，交给 `subagent { agent: "explore" }`，只把结论带回来；你自己只读当前这一步要改的文件。”
- **为什么**：功能上等价于 omp 的 checkpoint/rewind 和 CC“批准计划并清空规划上下文”：摸索用过的上下文不进主窗口。D25 里关于指南的工具输出约占一半（1.32M / 2.68M 字符）。
- **成本**：一句提示词，加上 ADR 0036 D1 的金样测试。风险是模型过度委派；explore 的结论上限是 4 000 字符。
- **验证**：评测集上比主会话的峰值输入 token 和压缩次数。

### 6.7 文件 checkpoint（收益中低，成本中，需要新 ADR，碰到 ADR 0022、0039、0020）
- **做什么**：照 opencode，用一个放在 `~/.radian/` 下的影子 git 仓库（ADR 0020 的目录，沙箱读不到），在每个用户轮开始时对工作区取一次树（`git write-tree`，只存对象）。`/rewind` 可以只回退文件，也可以文件和对话一起回退（对话部分复用 ADR 0022 的 fork：回退等于从那个轮间边界 fork）。
- **为什么**：让“试一个方案，不行就退”变便宜，回退这件事也不用占模型的轮次。比 CC 强的一点是 shell 改的文件也覆盖。Codex 把 undo 删了，说明这个功能的使用率或维护成本可能不划算（推断，没查到原因）。所以排在后面。
- **成本**：大仓库每轮取一次树的耗时要测（opencode 是异步 fork 出去取的，`step.ts:76`）。ADR 0039 规定宿主不跑 git hook，影子仓库要沿用这条。
- **验证**：快照时间放进 perf 基线；回退之后 `git diff` 为空。变异：shell 改的文件没进快照 → 测试失败。

### 6.8 fork 时带上 branch summary（收益低到中，成本低到中，修订 ADR 0022）
- 照 pi：fork 时可以选“从更早的边界另起，并把被放弃那一段的摘要带过去”（Goal/尝试过什么/为什么放弃）。好处是换方案时不会把失败的路再走一遍。复用 ADR 0018 的摘要器。优先级低，因为现在 Radian 的 fork 很少用（推断）。

### 先不做
- **完全自动的崩溃续跑**（opencode 式）：Radian 不是常驻服务，没有 `/goal` 时自动续跑可能误伤。并入 6.3。
- **Codex 式离线两阶段记忆管线**：Codex 和 omp 都默认关，成本高（每次启动后台跑模型，还要一个合并子 agent），收益没有量化。等 6.5 的 (a)、(b) 用一段时间、能看到记录数据之后再说。
- **plan mode**：ADR 0043 刚删了 mode。6.6 的 explore 加上 6.2 的 todo 已经覆盖了“先摸清再动手”的主要收益。

---

## 7. 未核实与说明

- dogfood 数字都是从 findings 表里抄的（D24–D26 是 owner 从 journal 统计的），原始 journal 不在本机，没有重新统计。§6.1 的收益估算是推断。
- “System 来源的提醒和 User 消息一样有效”**没有验证**。D25 的摘要（System 来源）没有起作用，所以 6.1 必须先 A/B，再定 ADR。
- Radian 实际有多少条记忆没有核实（不读家目录）。
- opencode 影子快照是否包含 shell 改的文件，是从“取整个工作区树”推断的，没有读 `git.tree.capture` 的实现细节。
- Codex 的 collaboration-mode 计划模式、omp 的 hindsight/mnemopi 记忆后端、omp 的后台任务都没有细查。
- CC 的所有条目来自 2026-10-18 抓取的官方文档（goal、checkpointing、sessions、memory、permission-modes），没有看二进制。CC 的 `/goal` 评估器用哪个模型、具体用什么 prompt，文档只说是“small fast model”。

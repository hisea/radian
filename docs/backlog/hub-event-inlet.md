# hub 的事件入口

- 状态：idea
- 涉及 ADR：0048、0043、0049、0024、0045
- 来源：`research/2026-11-04-agent-autoloop.md`「和 radian / arc / hub 的对照」「判断」

## 背景

hub 只收两类输入：owner 动作（`confirm`、`merge`、`reject`……）和 session 的 `prompt`。arc 的任务由人或 lead 加。外部世界发生的事（定时到了、PR 有新评论、CI 结果、一条告警）没有一条路进 hub 变成工作。`[hooks]` 和 `[hub.hooks] after_pending` 只出不进。

agent-autoloop 把这条路做成了一个独立中台：来源把事变成标准事件，规则文件决定交给哪个 Agent、哪个目录、什么权限，Agent 跑一轮就退，回执在最后一条消息里，`ask` 结束本轮并变成卡片，回答作为事件重新派活并 `--resume` 原会话。它证明了这件事能在零依赖、本机、无界面的前提下做小。

hub 已有 HTTP、bearer token、调度循环、会话的起停与 `answer`、`[hooks]`。缺的是四块：事件来源、事件到工作的规则、卡片回路、等人时的挂起与续接。

## 待决问题（未定）

1. **入口形态。** 一条 `POST /v1/repos/<name>/events` 加 `[hub]` 里的 cron 表？webhook 验签放哪（adapter 还是只收已验签的 HTTP）？事件要不要像 agent-autoloop 那样禁止携带 `cwd`、agent、权限字段，只带来源、编号、类型、对象、一句话、链接？
2. **事件变成什么。** 三个候选：（a）一个 `todo.proposed` 任务，owner 的 `confirm` 仍是闸门；（b）一次 hub 托管的 session `prompt`（这条路由今天就有）；（c）对已有任务的 `add_rework`。一条 PR 评论直接进 `review.rework_pending` 会不会绕过 owner 的 `reject`；CI 结果和 arc 自己的 gate 谁说了算。
3. **等人时的进程。** ADR 0024 的 `ask_user` 阻塞当前 turn，答案是工具结果；hub 托管的进程在有 pending question 时不按 `session_idle_secs` 退出。agent-autoloop 的做法是进程退出、问题落盘、回答后 `--resume`。要不要给 hub 一条"问题挂起即关进程、回答即续接并把答案作为工具结果交回"的路？这改的是 ADR 0024 和 0049 的决定，先改 ADR。
4. **卡片回路。** 今天的"问人"是 iOS app 经 hub 的 `answer` 路由；通知出口是 `[hooks]` 的 ntfy / Bark 行。只会发文字的通道要不要一个不跑脚本的表单链接当兜底（持有即可答，限期）？不做飞书按钮。
5. **幂等与熔断。** 同一来源同一编号只记一次；事件引发的事件要不要带跳数，上限多少；一个事项连续失败几次停。arc 已有 `max_attempts`、`max_auto_rework` 与唤醒闸门，是复用还是另计。
6. **先做 spike 还是先定设计。** 一小时的 spike：装 agent-autoloop，`rules.json` 把 `radian acp` 配成 `use: "acp"` 的 Agent，跑通 `agent-autoloop try`。结果告诉我们 `radian acp` 跨进程 `session/load` 在真实客户端下是否可用，以及 radian 的基础提示词能不能配合"回执在最后一条消息"。spike 不进仓库。

## 验收

idea 阶段只写最小形态：一条 cron 或一次 HTTP 事件能在 hub 里变成一个 `todo.proposed` 任务或一次 session `prompt`，重复事件只算一次，`radian arc show` 或 hub 的 `show` 路由能看到它来自哪个事件；ADR 0048 与 hub 手册在同一分支的 docs-only commit 里更新。

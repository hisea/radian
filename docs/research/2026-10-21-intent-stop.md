# 轮末「只说不做」停顿（intent-only stop）：成因、频率与对策

日期：2026-10-21。范围：radian 的一个会话在宣布下一步之后结束回合，没有工具调用，任务没完，然后一直空闲到有人来推。对照 pi 1.3.0 与 opencode v2.0.30 在同一任务、同一模型、同一 brief 下的表现，统计本机全部 radian journal 里这类停顿的频率，评估五类对策。只读数据与源码，不改代码。

## 0. 结论

- **这不是 `/loop` 或 `/goal` 引起的。** 出事的二进制（main `8b5e4557`）里没有这两条命令（`git grep "/loop"`、`"/goal"` 在 `crates/` 下无命中；`/goal` 在 `f5fecf86`、`/loop` 在 `55ad1808` 之后才合入，都在 ab6 开跑之后）。出事会话全是 `radian exec` 式的无头 worker，journal 里没有任何 loop/goal 记录。最早的同类停顿在 2026-10-17，比两条命令早四天。
- **也不是 API 协议、推理档位或 brief。** radian 与 opencode 都走 Responses API（opencode 的 `providerState.responseId` 是 `resp_…`），pi 走 Chat Completions；三家都是 `medium`；三份 brief 只差 worktree 名。opencode 与 pi 各 0 次停顿。
- **提示词改不好它。** 两代角色提示词都有「做完再停」的规则（旧版 `Work until the task is done … do not stop at a phase boundary`，新版 `End your turn only when the task is done, when you wait for a background job, or when you need the user`），停顿在两代下都出现。pi 和 opencode 的系统提示词里**没有任何**「继续干」的措辞，却不停顿。
- **radian 独有、而且和停顿共现的，是「回合中途结束、等通知唤醒」这条路（ADR 0037）**：后台 job、`shell` 描述里的「超过 60 s 自动转后台、完成会通知你」、完成通知以一条 user 角色的消息开启新回合。muse 的 9 次停顿里，4 次发生在通知开启的回合里（模型对通知像对聊天消息一样回一句就停），2 次紧跟在一次合法的等待回合之后，4 次卡在「写报告 / 发 herdr 通知」这最后一步。pi 没有后台执行，同一条 `cargo test` 它同步等了 288 s。
- **频率**：本机 42 个 muse 会话里 9 次确认停顿（7 个会话），占 muse 纯文本回合结束的 8.7%，占「先说一句再调用工具」那类叙述的 0.7%；pi 的 muse 会话 5 710 条助手消息里 0 次。有人看着时等 1–16 分钟，没人看着时等了 2 小时到一天。
- **建议**：先改提示词与通知文案（删掉角色里专为此加、却没起作用的 `End your turn only when …`；「工作区外可见要先问」加任务文本的例外；通知以「继续任务」结尾并修少换行；查 lead 记忆漏进 worker），并同时在 `Stop` 续跑路径（ADR 0045 §3）上内建保守检测 + 一次 `System` 续跑（模型判断，连续两次后 `blocked`）；两者是独立切片。backlog：`docs/backlog/turn-end-continuation.md`。

## 1. 案例

四个由 lead 核实的案例，加上同一会话里的第五个（它的回合结束是合法的——在等后台 job——但宣布的「顺便再读 ADR」也没做）：

| 会话（`~/.radian/sessions/<id>.jsonl`） | 回合 | 结束（UTC） | 最后一句 | 回合由谁开启 | 本轮模型调用数 | 等人多久 |
|---|---|---|---|---|---|---|
| `18e069c036056550-166f3`（mcp-b2） | t-2 | 02:48:56 | `Acceptance is fully green. Now the docs-only commit — updating the ADRs.` | job 完成通知 | 1 | 16.3 min |
| `18e06cb8a11347b8-127dc`（tui-new） | t-1 | 03:18:57 | `Design is settled. Updating the progress file, then implementing.` | 用户 | 68 | 6.3 min |
| `18e07555d80877e8-879`（ab6） | t-1 | 06:09:25 | `While the suite runs, I'll re-read the exact ADR passages I'll need to update …` | 用户 | 373 条消息 | 4.4 min，被通知唤醒（合法等待） |
| 同上 | t-2 | 06:14:01 | `All 128 test binaries pass with zero failures. Now the docs-only ADR updates.` | job 完成通知 | 4 | 46 s（lead 发 `Continue.`） |
| 同上 | t-3 | 06:15:40 | `Both commits landed. Writing the report.` | 用户 `Continue.` | 9 | 113 s（lead 再发 `Continue.`） |

ab6 的四个回合里三个以「宣布下一步」结束，只有最后一个（`Done. …`）是真结束。pi 的 ab6 会话只有一个回合：05:47:52 收任务，06:10:01 一条 `Done.` 结束，中间 175 条助手消息全部带工具调用。opencode 同样：206 条助手消息里只有最后的 `Done.` 是纯文本。

## 2. 机制：radian 为什么在这里结束回合

`crates/agent-core/src/turn.rs` 的回合循环：模型回复带工具调用就派发并继续；没有工具调用且 `stop_reason == EndTurn` 时，除非 steering 队列里有待处理消息，否则 `RunStatus::Completed`，回合结束。核心不看文本内容，不重投，这是设计（ADR 0017：核心不判断什么算「有意义」）。

回合结束后只有三条路能再开一轮：用户消息；后台 job 的完成通知（ADR 0037 决定 5、ADR 0011 L5：`followup`，`source = System`，受 10 分钟 10 次的唤醒闸门）；配置了 `Stop` hook 且它 exit 2（ADR 0045 §3，同一闸门）。`/loop`、`/goal` 是第四条，要用户显式开。

所以停顿的直接原因是模型回了一条纯文本；radian 之后的行为（等人）是 ADR 定好的。问题是**为什么同一个模型在 radian 里会回纯文本，在 pi、opencode 里不会**。

## 3. 三家对照（ab6：同任务、同模型 muse-spark-1.3-contributor、同 brief、同机同时起跑）

### 3.1 各家发给模型的东西（都读了源码或二进制）

| | radian `8b5e4557` | pi 1.3.0（`@earendil-works/pi-coding-agent`） | opencode v2.0.30 |
|---|---|---|---|
| 系统提示词 | `crates/radian-host/prompts/default.md`，9 条规则，含 `End your turn only when the task is done, when you wait for a background job, or when you need the user.`；加 AGENTS.md、5 条 memory | `dist/core/system-prompt.js`：`You are an expert coding assistant operating inside pi…`，工具列表、`Be concise`、`Show file paths clearly`、pi 文档路径、AGENTS.md；**没有**任何关于何时结束回合的话 | `runner/prompt/system.txt`（二进制 `strings` 核对一致）：`You are an AI agent running in OpenCode… Help the user accomplish their goals`，`Prefer parallelizing independent tool calls`，沟通与代码库两段；**没有**关于结束回合的话 |
| 工具 | read、write、patch、glob、grep、shell、job、skill、subagent（+ask）；`shell` 描述说「没给 `timeout_secs` 的命令超过 60 s 转后台，调用立刻返回 job id，**完成会通知你**」；`job` 描述说「完成通知会告诉你；用 notice 而不是 wait」 | read、bash、edit、write；bash 同步执行，有超时，**没有后台/通知** | read、edit、write、shell、grep 等；同步执行 |
| 回合结束语义 | 纯文本 `EndTurn` → `Completed`，等人；通知/`Stop` hook 可唤醒 | `agent-loop.js` `runLoop`：没有工具调用且没有排队的 follow-up/steering 就 `agent_end`；扩展可通过 `finishTurn` 返回 `continue`（本机装的扩展里未见实现，未逐一核对） | `runner/step.ts`（master 源码，v2.0.30 未逐行核对）：`needsContinuation` 由待执行的工具调用决定；纯文本 `stop` 结束 drain |
| API | Responses（`agent-provider-openai-responses`：`store:false`、`include: reasoning.encrypted_content`、`instructions`、`reasoning.effort`） | Chat Completions（`~/.pi/agent/cache/opencode-go-models.json`：`"api": "openai-completions"`，baseUrl `opencode.ai/zen/go/v1`） | Responses（`providerState.responseId = resp_…`） |
| 推理 | medium，加密推理原样回放 | medium，`thinkingSignature` 回放 | medium |
| 自动续跑 | 无（通知与 `Stop` hook 之外） | 无 | 无 |

### 3.2 行为

| | radian | pi | opencode |
|---|---|---|---|
| 助手消息 | 177 | 176 | 206 |
| 带工具调用 | 173 | 175 | 203 |
| 先说一句再调用工具（叙述） | 31 | 37 | 38 |
| 纯文本结束回合 | 4（3 次停顿 + 1 次 `Done.`） | 1（`Done.`） | 1（`Done.`，另 2 条是失败调用的空消息） |
| 人工推动 | 2 次 `Continue.` | 0 | 0 |
| 最长单次工具等待 | 后台 job 4m34s（回合结束等通知） | `cargo test …` 同步 288 s | — |
| 用时 | 30 min（含两次等人） | 22 min | 23 min |

三家「叙述再调用」的比例一样（18–21%），说明模型在哪里都会先说 `Now the herdr label.` 再调工具；差别是 radian 里这句话之后有 3 次没跟工具调用。

### 3.3 能排除的与剩下的

排除：API 协议（opencode 也是 Responses，0 次）；推理档位（都 medium）；brief（`diff` 只差名字）；「继续干」措辞（pi、opencode 一句没有，0 次；radian 两代都有，照停）。

剩下的 radian 独有差别，按与停顿的共现强弱排：

1. **回合可以在任务中途合法结束**（后台 job + 通知唤醒）。工具描述与角色规则都告诉模型「等 job 时结束回合是对的、会被叫醒」。pi 没有这个概念：同一条 `cargo test` 它同步等到底。muse 的 9 次停顿里 6 次在一次合法等待之后一两个回合内（见 §4.3）。
2. **完成通知是一条 user 角色的消息**（`source = system`，`[background job … finished: exited 0 after 4m34s]\noutput: …`）。模型对它的回应是「聊天式」的一句话（`All 128 test binaries pass with zero failures. Now the docs-only ADR updates.`），然后停——和回答一条用户消息一模一样。通知开启的 50 个回合里 7 个以「宣布下一步」结束（14%），用户开启的 232 个里 16 个（7%）。
3. **人工 `Continue.` 也是一条短 user 消息**，模型同样回一句就停（ab6 t-3）。
4. **「写报告 / 发通知」是任务的最后一步**，模型把「报告」当成了对话里的最终答复：`Writing the report.`、`Sending the notice.`、`Notifying the lead's pane.`、`Both commits landed. Writing the report.`——4/9。
5. 工具数与描述长度（radian 9–10 个工具，pi 4 个）：无法从数据里分离，只记一笔。

未验证、只作假设：同一上下文里已经出现过一次「说一句就结束回合」之后，模型更容易再来一次（ab6 t-1 → t-2 → t-3 连续三次）。

### 3.4 基础提示词逐条对比（ab6 实际发出的内容）

pi 的 system 消息存在它的会话 jsonl 里；radian 的按 ADR 0036 五层拼出，再加 `shell`、`job` 的工具描述（模型同样看得到）。

**pi**：身份一句（`expert coding assistant operating inside pi … help users by reading files, executing commands, editing code, and writing new files`）；工具列表；13 条规则，全是工具用法（`Use bash for file operations like ls, rg, find`、`Use read to examine files instead of cat or sed`、edit 的 `oldText` 规则、`Be concise in your responses`、`Show file paths clearly`）；pi 文档路径；AGENTS.md；skills；cwd。**没有一个字**提到回合、等待、后台、通知或何时结束。`bash` 的 `timeout` 参数写的是 `no default timeout`，模型就同步等。

**radian**：`<runtime>`（full：无沙箱、网络开、无审批弹窗、项目说明文件是数据）；`<role>` 9 条；AGENTS.md；5 条 memory；`<session>`（cwd、日期、os、shell、git、model）；工具描述 9–10 个。

radian 有、pi 没有、且可能导致停顿的内容，按证据强弱排：

| # | radian 独有的内容 | 为什么可能导致停顿 | 证据 |
|---|---|---|---|
| 1 | 四句话合起来教模型「等待 = 结束回合」：角色 `Do not use sleep or polling to wait`、`End your turn only when … when you wait for a background job …`；`shell` 描述 `The call returns at once with a job id. You will be notified when the command finishes`；`job` 描述 `A completion notice tells you when a running job ends. Prefer the notice over wait` | pi 的模型没有「任务中途结束回合」这个动作；radian 的模型被明确告知它是对的。用过一次（合法等待）之后，「说一句就停」是同一个动作的误用 | 强：9 次里 6 次在一次合法等待之后一两轮内，4 次在通知开启的回合里 |
| 2 | 角色 `Ask the user before an action that you cannot undo or that is visible outside the workspace. Examples … publishing` | 给 lead 的 pane 发 `herdr agent prompt` 正是「工作区外可见」的动作。`Sending the notice.`、`Notifying the lead's pane.` 两次停顿恰好停在发通知之前，像在「问」却没写出问句 | 中：2/9 位置吻合；模型没真问，是推测 |
| 3 | 角色 `Keep your answers short`，加上完成通知以 user 角色消息到达 | pi 也有 `Be concise`，单独不是差别；radian 里模型是在「回答一条用户消息」（通知），短回答 + 用户消息 = 一行就停。通知开启的回合 14% 以宣布下一步结束，用户开启的 7% | 中 |
| 4 | 5 条 memory，内容是 lead 角色的指令（拆 slice 派给 task agent、及时 bug report、「计划不阻塞时同一轮里直接开始」） | lead 会话的记忆漏进了 worker 会话。其中一条正是反对停顿的，所以不是成因，但角色噪音可能让模型不确定自己是执行者还是调度者 | 弱：污染属实，影响未验证 |
| 5 | `End your turn only when …` 本身 | 列出三个「允许结束」的理由，等于告诉模型结束回合是一个要选理由的动作；pi 从不提这个动作。停顿时模型没满足任何一条，这句既不是成因也没起作用 | 弱 |

pi 有而 radian 没有的内容里，没有一条和回合有关。pi 不停顿不是因为它说了什么，而是因为它什么都没说：模型只剩「调工具」和「交最终答复」两个动作。

### 3.5 七家 harness 怎么说明后台命令与通知

来源：pi、omp、opencode、Claude Code、Codex 看本机安装的二进制或源码；Gemini CLI 看 master 源码（`packages/core/src/tools/definitions/dynamic-declaration-helpers.ts`、`tools/shell.ts`）；Codex 的工具描述原文在二进制里不是明文，只拿到结构。

| harness | 工具层怎么说 | 结果 / 通知怎么到模型 | 缺省自动转后台 |
|---|---|---|---|
| pi 1.3.0 | 没有后台概念。`timeout: Timeout in seconds (optional, no default timeout)` | 同步阻塞 | 否 |
| opencode v2.0.30 | 描述：`Commands accept an optional timeout, background commands have no timeout by default. Background commands return immediately, and you will be notified when they complete.` 参数：`Run the command in the background and return immediately (useful for dev servers and long-running builds)… You will be notified when it completes. DO NOT poll for completion.` | 转后台时的返回文本是一整段指令：`You will be notified automatically when the command finishes. The notification will include the command's output. … DO NOT poll for completion, even if you need the final result to continue. Repeatedly sleeping and reading or searching the output file is polling, not useful work. … Keep working on anything that does not depend on the result. If you have nothing else to do, end your response; you will be resumed automatically when the command finishes.` | 否；用户可手动 move running work to the background。ab6 里 32 次 shell 全是前台，timeout 60–600 s |
| Claude Code | `run_in_background runs the command detached: it keeps running across turns and re-invokes you when it exits. No & needed. You will be notified when it exits — do not poll its output file while waiting. Foreground sleep is blocked; use Monitor with an until-loop to wait on a condition.` 前台缺省 120 s、上限 600 s | 任务通知是系统提醒；系统提示另有 `when harness-tracked work finishes, you are re-invoked automatically` | 否 |
| omp 18.x | `bash.md`：`async: true defers a finite command's result; it does not extend timeout.`；开了 autoBackground 时多一句 `Long foreground calls may auto-background and deliver later. Need inline? Raise timeout.`；返回 `Backgrounded as job X; result will be delivered automatically.` | `custom` 角色消息，包在 `<system-notice>Background job X has completed. Resume your work using the result below. …</system-notice>` 里 | 可选，缺省 false，阈值 60 s |
| Gemini CLI（master） | `To run a command in the background, set is_background to true. Do NOT use & …`；参数：`Set to true if this command should be run in the background (e.g. for long-running servers or watchers). … allowed to run for a brief moment to check for immediate errors, and then moved to the background.` | 返回 `Command is running in background. PID: N. Initial output: …`；工具文本里没有「会通知你」 | 否；后台定位是服务与监视器 |
| Codex 0.159.1 | 统一 exec：`exec_command` + `write_stdin`，`yield_time_ms`（缺省由 `default_exec_yield_time_ms` 配），「background terminal」，`background_terminal_max_timeout` 300 s | 命令留在终端会话里，模型在本回合内用 `write_stdin` 或再次 yield 回去看：设计上的轮询，没有开新回合的完成通知 | 否 |
| radian `8b5e4557` | `shell`：`Without timeout_secs, a command that still runs after the threshold moves to the background. The default threshold is 60 s. … The call returns at once with a job id. You will be notified when the command finishes.`；`job`：`Prefer the notice over wait.`；角色：`Do not use sleep or polling to wait` 与 end-turn 句 | user 角色消息 `[background job … finished: exited 0 after 4m34s] output: … Read its output with job.` | **是，缺省 60 s** |

三点启发：

1. **只有 radian 缺省自动转后台。** 别家都要模型每次显式选 background，并把它定位成 dev server、长 build。radian 任何没写 `timeout_secs` 的命令过 60 s 就转后台，等于每跑一次 `cargo test` 就把模型推进「结束回合等通知」模式；opencode 的模型给自己 300–600 s 超时同步等完，pi 等了 288 s。结论：取消自动转后台（ADR 0037 §2），后台只留显式 `background: true`，并限定为服务、监视器和超过 600 s 的命令；前台缺省超时 300 s（§6）。
2. **opencode 的措辞把「结束回合」写成最后一招**（`Keep working on anything that does not depend on the result. If you have nothing else to do, end your response; you will be resumed automatically.`），而且放在**转后台时的返回文本**里，不在系统提示里：模型只在真的进入等待时才看到。radian 的角色句把它写成三个平级理由之一，且每轮都在。
3. **通知不要像用户消息。** omp 用 `custom` 角色加 `<system-notice>… Resume your work using the result below.`，Claude Code 用系统提醒。radian 的通知是 user 角色，后面只跟 `Read its output with job.`。角色改不了的话，文本至少改成 `Resume the task using this result. Do not reply to this notice.`。

后台使用的实际数据（本机 radian journal）：shell 调用 6 690 次前台、570 次显式 `background: true`、49 次被自动转后台（muse：1 955 / 227 / 15）。9 次停顿前面的 job 全是显式后台。所以模型已经偏好后台，偏好来自工具描述；取消自动转后台与停顿没有直接关系，真正的暴露面是每一次显式后台之后的回合结束。pi 与 opencode 同步等完（最长 288 s），总用时还比 radian 短 7–8 分钟。

两家已有「只说不做」的对策：omp 的 `features.unexpectedStopDetection`（缺省关）用小模型给纯文本停顿分类（`Classify whether this assistant message is an unexpected stop: it says it will act, continue working, or call a tool, then ends without doing so`），命中就注入一条 developer 消息 `You said you would continue with a tool call or action but stopped. Continue now. Attempt #n/3`，上限 3 次；Claude Code 走提示词：`Before ending your turn, check your last paragraph. If it is a plan, an analysis, a question, a list of next steps, or a promise about work you have not done, do that work now with tool calls.` 前者是 §5 (b)/(c) 的形状，后者是 (a) 的形状。

## 4. 频率：本机全部 radian journal

数据：`~/.radian/sessions/*.jsonl` 348 个文件，其中 133 个会话有完整回合，311 个 `TurnFinished`，286 个 `completed`（13 failed、12 cancelled）。一个 `completed` 回合的最后一条助手消息都是纯文本（否则回合不会结束），所以问题是「这些纯文本里哪些是停顿」。

### 4.1 人工标注的真停顿

标准：最后一句宣布自己接下来要做的动作，且（a）下一条消息是人工 `Continue`/同义推动，或（b）会话到此为止、宣布的事没做。

| 会话 | 模型 | 结束 | 最后一句 | 回合开启者 | 之后 |
|---|---|---|---|---|---|
| `18df6999fb9422` | muse | 10-17 20:30 | `Full workspace re-run is green. Confirming the details.` | 用户 | 193 s 后 `Continue the task in .radian/task.md now …` |
| `18df69f318a673` | muse | 10-17 20:09 | `Reading the approval before I build.` | 用户（lead 批准） | 1 492 s 后 `Continue the task …` |
| `18df6b82d355b2` | muse | 10-17 20:37 | `Now the three tests, appended after the existing \`jobs\` test:` | 用户 | 会话结束，测试没写 |
| `18e041977bbbba` | muse | 10-20 14:33 | `Notifying the lead's pane.` | job 通知 | 12.6 h 后才有新消息 |
| `18e049c9e1bb29` | muse | 10-20 16:43 | `Report's in place and the tree is clean. Sending the notice.` | job 通知 | 6 954 s 后下一个任务 |
| `18e069c0360565` | muse | 10-21 02:48 | `Acceptance is fully green. Now the docs-only commit — updating the ADRs.` | job 通知 | 975 s 后 `Continue: …` |
| `18e06cb8a11347` | muse | 10-21 03:18 | `Design is settled. Updating the progress file, then implementing.` | 用户 | 379 s 后 `Continue now: …` |
| `18e07555d80877` | muse | 10-21 06:14 | `… Now the docs-only ADR updates.` | job 通知 | 47 s 后 `Continue.` |
| `18e07555d80877` | muse | 10-21 06:15 | `Both commits landed. Writing the report.` | 用户 `Continue.` | 113 s 后 `Continue.` |
| `18df32914a816b` | gpt-5.6-sol（lead 会话） | 10-17 05:48 | `接下来审 footer usage 的 byte-identical 输出 …` | job 通知 | 92 s 后 lead 说「你上一轮写了'接下来审 footer usage'但停在 idle」 |
| 同上 | 同上 | 10-17 06:01 | `… 接下来需要检查这些共享文件的语义合并。` | job 通知 | 992 s 后「你从 06:25 起空闲了 17 分钟」 |

muse：9 次 / 42 个会话（7 个会话出事）；另一个模型（GPT lead 会话）2 次，所以不是 muse 独有。muse 在 radian 里 5 844 条助手消息、1 333 条「叙述再调用」、103 次纯文本结束回合：停顿 = 0.7% 的叙述、8.7% 的纯文本结束。pi 的 muse：5 710 条助手消息、1 127 条叙述、54 次纯文本结束（46 次是会话末尾的完成报告，8 次之后紧接新任务），**0 次停顿**。

时间线：muse 在 radian 的第一个会话是 10-17 16:40，三小时后就出现第一次停顿。所谓「最近才出现」与 muse 的引入同步，与 `/loop`、`/goal` 无关。

### 4.2 其他纯文本结束的构成

对 286 个 `completed` 回合按最后一句分类（规则见 4.4）：

| 类别 | 数量 | 之后由谁开启下一轮 | 说明 |
|---|---|---|---|
| 完成报告 / 一般回答（other） | 181 | 用户 75、通知 9、无 97 | 正常结束 |
| 等待（waiting，明说在等 job） | 53 | 通知 39、用户 12、无 2 | 39 次被通知正确唤醒；用户开启的 12 次里 3 次是「通知丢了」（`Your background job has finished but its notice …`，另一个 bug，`job-notice-lost-on-write-failure.md`），其余是 lead 会话的聊天 |
| 问句（question） | 29 | 用户 20、无 9 | 正常结束 |
| 宣布下一步（intent） | 23 | 用户 12、通知 3、无 8 | 其中 12 条是上表的真停顿 |

### 4.3 与后台 job 的关系

- 286 个 `completed` 回合里 60 个结束时有后台 job 在跑；其中 51 个被通知唤醒，9 个等到了用户消息（3 个通知丢失）。
- 9 次 muse 停顿发生时都**没有** job 在跑（所以不是「在等」）。但 4 次的回合是由通知开启的，另 2 次（mcp-b2 t-2 的前一轮、ab6 t-2 的前一轮）紧跟在一个等待回合之后。
- 通知开启的回合：50 个，模型调用数分布 1 次 2 个、2 次 4 个、3 次 6 个、4 次 4 个、5 次 8 个、≥6 次 26 个。1–4 次就结束的回合里，停顿占了 3 个。

### 4.4 一个检测器在这批数据上的表现

规则（对最后一句）：以动名词（`Updating`、`Writing`、`Confirming`…）、`Now`/`Next`/`Then`/`Let me`/`I'll`/`I will`/`Time to`，或「接下来 / 现在 / 然后 / 下一步」开头；句末没有问号；排除「要不要 / 说一声 / 告诉我」等问人的句式；排除明说在等 job 的句子。

- 标出 23 / 286。真停顿 12 条全部命中（召回 100%）。
- 误报 11 条，三类：`Next: the reviewer / p1 reviews the branch …`、`Next: nothing pending …` 一类（动作主体是别人，4 条）；`Nothing further unless p1 wants changes.`（`Nothing` 被当动名词，1 条）；`现在重启你的 radian 就能试 …`、`下一步应先提交 adapter …`（给用户的建议，长句，3 条）；lead 会话里由通知开启、宣布下一步后又被另一条通知接上的 3 条（不算停顿）。
- 再加三条过滤——全文不超过约 160 字符、不以 `you`/`p1`/`lead`/`你` 为动作主体、`Nothing` 不算动名词——本机数据上误报剩 1–2 条。样本小（12 正例），不能当精度数字用，只能说明：**纯规则够做粗筛，不够直接决定续跑。**

真停顿的共同形状更简单：**短**（9 条 muse 停顿里 8 条不超过 80 字符，最长的 112 字符）、**无问号**、**动作主体是「我」**、**本轮或前几轮刚结束一次等待或刚收到一条短消息**。

## 5. 对策评估

| 选项 | 内容 | 证据说它有用吗 | 误报风险 | 成本 | 怎么证明 |
|---|---|---|---|---|---|
| (a) 先改提示词与通知文案 | 删掉角色里的 `End your turn only when …`（§3.4 第 5 项，专为此加的，没起作用）；「工作区外可见要先问」加例外（任务文本里要求的通知算已获许可）；完成通知以「继续任务」结尾并修少换行；查 lead 记忆漏进 worker 的注入。备选：把等待写成固定句式 `waiting for job <id>` | 中。两代规则都没挡住停顿，所以不指望归零；但 §3.4 的 1、2、5 三项是 radian 独有的诱因，去掉它们是最便宜的一步 | 无 | 最低 | 同一任务集 A/B 各 3 次，数人工 `Continue` 次数（ab6 metrics 已有这列）与纯文本中途结束的次数 |
| (b) 轮末守卫（harness） | 检测器 + 一次自动 `System` `followup`「继续」，受唤醒闸门，三路可见 | 直接覆盖全部 12 例（都在 `Completed` 回合末、都能被规则命中） | **主要风险**。纯规则直接续跑：11/23 误报，每次误报是一轮计划外的工作。把判断交给模型（续跑消息写明「上一条只宣布了下一步却没调用工具；没完就继续，完了或要问用户就一行说明」）：误报代价变成一次短调用 + transcript 一行，不会做计划外的事 | 中：检测器、meta、闸门接入、TUI/herdr 可见、测试 | mock provider 单测（命中 → 恰好一条 System followup；`Done.`/问句/等待 → 无；连续两次后 `blocked`）；变异去掉检测器/上限；A/B 数人工推动次数与误续跑次数 |
| (c) 内建到 `Stop` 路径 | 同 (b)，但走 ADR 0045 §3 已有的 `Stop` 续跑：同一闸门、同一降级、同一「只在 `Completed` 跑」；要给 `Stop` 的 stdin 加上一条助手消息的文本 | 同 (b) | 同 (b)；另一个好处是用户能用自己的脚本换掉或关掉内建规则 | 比 (b) 低：唤醒闸门、降级可见、每回合一次都已经有了 | 同 (b)，加 `Stop` stdin 字段的测试 |
| (d) 模型/参数 | `tool_choice: required` 或改推理档位 | `required` 会让回合永远结不了，不可用；pi 同 `medium` 0 次，档位不是杠杆；协议不是杠杆（opencode 也是 Responses） | — | — | 不建议做 |
| (e) 学 pi/opencode | 它们什么都没做：没有后台执行，模型同步等 `cargo test`，所以没有「中途结束回合」这个动作 | 它们的 0 次就是证据 | 放弃后台 job 会让 ADR 0037 的全部收益（长命令不占回合、超时不杀）作废 | 不可逆的产品决定 | 不建议整体回退；但可以**缩小**诱因：通知文案以「继续任务」结尾、不把通知写得像需要回复的消息 |

两条独立的小改动，不依赖上面的决定：

- **通知文案**（ADR 0037 决定 5 的形状）：`completion_message`（`crates/radian-host/src/job_notify.rs`）在尾部为空时把 `Read its output with job.` 直接接在 `output: available through job` 后面，少一个换行（ab6、tui-new 的 journal 里都能看到 `jobRead`）；并且通知里没有一句「继续任务」。改成以 `Continue the task; do not end the turn only to report this result.` 结尾，直接针对 4/9 的情形。
- **角色规则**保留现在那一句，不再加长：证据说它不是杠杆。

## 6. 建议

A（提示词与通知文案）和 B（内建续跑）都做，两个独立切片；B 不等 A 的度量，因为显式后台之后的回合结束是每天都在发生的暴露面（§3.5 的数据）。

1. **第一步，提示词与通知文案**（`docs/backlog/turn-end-continuation.md` A 部分）：
   - **删掉**角色里的 `End your turn only when the task is done, when you wait for a background job, or when you need the user.`。它是 `9aa6fff5` 为这个问题加的，ab6 带着它照样停了 3 次；更早的长版规则（`do not stop at a phase boundary`）在 10-17 也没挡住；pi、opencode 对回合一字不提而 0 次。它唯一的正面作用——告诉模型等 job 时可以结束回合——`shell`、`job` 的描述已经说了（`You will be notified`、`Prefer the notice over wait`）。它的副作用是把「结束回合」变成一个有理由可选的动作。
   - 备选（只在之后要做守卫、需要可识别的等待标记时再加）：`To wait for a background job, end your reply with exactly: waiting for job <id>.` 一句，不再列结束理由。
   - 「工作区外可见要先问」加例外：任务文本里要求的通知与提交不用再问。
   - 完成通知末尾加 `Continue the task; do not end the turn only to report this result.`，修掉尾部为空时少换行的 bug（`job_notify.rs` 的 `completion_message`）。
   - 查 memory 注入：lead 会话的记忆为什么进了 worker 会话（同一 project scope），是否该按角色或会话隔离。
   - `shell` 转后台时的返回文本借 opencode 的那段（§3.5）：先做不依赖结果的事，实在没事再结束回复，会被自动唤醒；通知文本以「Resume the task using this result. Do not reply to this notice.」收尾。
   - 取消自动转后台（ADR 0037 §2）：七家里只有 radian 缺省这样做，它是把模型推进「结束回合等通知」模式的独有推力；后台只留显式 `background: true`。前台缺省超时 300 s、上限 600 s 不变；`background` 参数限定为服务、监视器和预计超过 600 s 的命令，测试与构建在前台跑；`job` 描述去掉 `Prefer the notice over wait`。
   - 角色正文的内容清单写在 ADR 0036 里，落地时同一分支的 docs-only commit 改 ADR。
2. **量**：同一任务集 A/B/C 各 3 次，看人工 `Continue` 次数和「无 `waiting for job` 标记的纯文本结束」次数。
3. **B，内建续跑**（backlog B 部分）：`Stop` 路径上的保守规则粗筛 + 一条固定措辞的 `System` 续跑，让模型自己判断是继续还是一行说明已完成；每回合一次、连续两次后 herdr `blocked`；计入唤醒闸门；`Stop` stdin 加 `last_assistant_message`，配置可关。
4. 不动模型参数。

## 附：方法

- radian：按 `TurnStarted`/`TurnFinished` 切回合，取回合内最后一条 `MessageAppended.assistant`，看是否带 `tool_calls`；下一条 `InboxEnqueued` 的 `source` 与时间差；`BackgroundJobStarted`/`Settled` 算结束时在跑的 job 数。
- pi：`~/.pi/agent/sessions/**/*.jsonl` 的 `message` 记录，`role = assistant` 且 content 无 `toolCall`、`stopReason = stop`，再看下一条 user 消息。
- opencode：`~/.local/share/opencode/opencode.db` 的 `session_v2` / `session_message`（v2 存储），`type = assistant` 的 `content` 里有无 `tool`。
- 源码：pi `dist/core/system-prompt.js`、`pi-agent-core/dist/agent-loop.js`、`~/.pi/agent/cache/opencode-go-models.json`；opencode `packages/core/src/session/runner/{step.ts,prompt/system.txt}`（master）与安装二进制 `strings`；radian `turn.rs`、`hooks.rs`、`job_notify.rs`、`wake_gate.rs`、`shell.rs`、`default.md` 在 `8b5e4557`。

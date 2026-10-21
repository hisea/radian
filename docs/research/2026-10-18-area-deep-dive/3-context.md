# 深挖 3：Context（上下文管理）—— Radian 与五个对手

日期：2026-10-18 · 代码版本：Radian `356df1e`；Codex `8c07694`（2026-10-18）；opencode `63c1a27`（2026-10-17）；pi-mono `482c138`（2026-10-18）；omp 18.12.1 npm 包；Claude Code 只看官方文档（每条注明）。
复用：系统提示词的组成、各家的第 0 层事实、易变事实放最后这几点，见 `docs/research/2026-10-14-system-prompts.md`（下称「提示词调研」）。本文不重复这些，只补**新版本的变化、数字，以及压缩和缓存**。

出处前缀：`rad/` = `<repo>`；`codex/`、`oc/`（= `opencode/packages/core/src`）、`pi/`（= `pi-mono/packages`）、`omp/`（= `npm-_oh-my-pi_pi-coding-agent/package/src`）、`omp-core/`（= `npm-_oh-my-pi_pi-agent-core/package/src`）都在 `scratchpad/comp/` 下。

---

## 1. 一句话结论

Radian 的**前缀稳定性**是六家里最严格的：0a/1/2/3 层在一个进程内只算一次，memory 块按会话固定，中途变化都作为消息追加。上下文文件放在 system 里，压缩后也不会丢。弱点在两个"缓存之外"的地方。第一，**缓存寿命**：只用 5 分钟 TTL，没有 `prompt_cache_key`，而实测 `job_output` 的中位等待是 361 s，超过 TTL，等完之后的那次请求缓存已经过期。第二，**压缩的成本和失忆**：摘要请求换了 system，把历史压成纯文本，读不到缓存；没有"只清旧工具输出"的轻量一档；压缩后也不补读文件、不补回 skill 正文。能直接提高命中率、减少 token、减少重查的改动按收益排是：1h TTL 或保温、`prompt_cache_key`、摘要请求共享前缀、轻量清理档、压缩后补读、截断输出存档。

---

## 2. 对比表

| 维度 | Radian | pi | Codex | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 固定前缀大小（system + 工具定义） | 角色 3 845 B（`rad/crates/radian-host/prompts/default.md`）；12 个编码工具定义**实测 14 297 B**，其中 `shell` 3 861 B、`job_output` 2 275 B；另有 memory/ask/skill/subagent 约 7.9 KB（**按源码估算**）；memory 块上限 2 000 tok、skills 段 1 000 tok。合计约 7–10k tok，不含 AGENTS.md | 默认 4 个工具（`pi/coding-agent/src/core/settings-manager.ts:85`），提示词很短（提示词调研 §1.2） | `instructions_template` 19–22 KB（`codex/codex-rs/models-manager/models.json:76`，按模型不同） | 文档的示意值：system 4 200 tok、环境 280 tok，MCP 默认延迟加载（只列名字，约 120 tok）（[context-window](https://code.claude.com/docs/en/context-window)） | 尾部 `<project-context>` 等（提示词调研 §1.3） | 按模型选 `.txt`（提示词调研 §1.5） |
| 上下文文件 | 全局 + 工作区到仓库根；单文件 64 KiB、合计 128 KiB，超限先丢外层；**只读一次**，子目录不发现（`rad/crates/radian-host/src/context.rs:27-29,104`） | 一路走到文件系统根（ADR 0036 背景 3） | 项目根到 cwd，合计 **32 KiB**（`codex/codex-rs/config/src/config_toml.rs:75`）；作为 **user 角色**消息（`codex/.../context/user_instructions.rs:22-31`）；在请求边界按 world-state diff 刷新（`codex/.../agents_md_manager.rs:64`） | 根目录 CLAUDE.md 启动时加载；**子目录里的在读到该目录文件时才加载**；建议不超过 200 行；`@import` 最多 4 跳（[memory](https://code.claude.com/docs/en/memory)） | 尾部 `<dir-context>` | 全局 + 逐级 |
| 缓存断点 | Anthropic：最后一个工具、system 末尾、最后一条消息的最后一个可缓存块，共 3 个（`rad/crates/agent-provider-anthropic/src/request.rs:84-112,134`）；**只用 5 分钟 TTL**（`:27-29`） | 同样 3 处（`pi/ai/src/providers/compat-schema.ts:323`）；可选 long retention：Anthropic `ttl:"1h"`、OpenAI `24h`（`pi/ai/src/api/openai-completions.ts:833,1087`） | 无断点（OpenAI 自动缓存） | 分三层（system+工具 / 项目上下文 / 对话）；订阅用户的主对话默认 1h TTL，API key 默认 5m，可用 `promptCacheTtl` 设置（[prompt-caching](https://code.claude.com/docs/en/prompt-caching)） | 剪枝时考虑 Anthropic 20 块的回看窗口（`omp-core/compaction/pruning.ts:12-20`） | 按 provider 的插件 |
| OpenAI 缓存路由 | **不发 `prompt_cache_key`**（全仓库 grep 无结果） | `prompt_cache_key = sessionId`（`pi/ai/src/api/openai-responses.ts:362`） | `prompt_cache_key = session_id`，子 agent 用 `{source}:{parent}`（`codex/codex-rs/core/src/client.rs:646-657`）；websocket 增量请求，比对请求属性决定能否复用（`client.rs:428-456`） | — | — | 子会话共用父会话的 affinity id（`oc/session/affinity.ts:6-8`） |
| 会话中途的变化 | 一律作为 System 消息追加；`<session>` 只在 `/model`、`/thinking`、`/reload` 时重算（ADR 0036） | 记录里的 `SystemMessage` 带 `sections` 增量与 `toolsAdded/Removed`；provider 支持的话原位发送（`pi/ai/src/types.ts:536-560`） | world-state 分段，按 diff 渲染，分 Prefix/Standalone/Mergeable 三种放置（`codex/.../context/world_state/mod.rs:62-75`） | 文件改动、plan、skill 都作为消息追加；CLAUDE.md 中途改了要到 `/clear` 或 `/compact` 才生效（[prompt-caching](https://code.claude.com/docs/en/prompt-caching)） | 日期和 cwd 放在第一条 user 回合（提示词调研 §4.2） | 指令变化只发 diff（提示词调研 §4.2） |
| 自动压缩触发点 | `T > B`，`B = min(I, C−R)`，`R = max(min(16384, C/4), max_out)`（ADR 0018:33-37），即窗口减去约 16k | `> window − 16384`（`pi/coding-agent/src/core/compaction/compaction.ts:126-129,267-269`） | 窗口的 **90%**，可配置得更低（`codex/codex-rs/protocol/src/openai_models.rs:529-540`） | 默认到模型上限；1M 窗口约在 967K；`/autocompact` 可设 100K–1M（[model-config](https://code.claude.com/docs/en/model-config)） | 百分比或 token 阈值，可按模型设置（`omp/config/compaction-threshold.ts:1-45`） | `window − max(10%, 16k)`（`oc/session/compaction.ts:911-917`） |
| 谁来摘要、是否共享前缀 | 当前模型；**换一个摘要专用 system，历史压成纯文本，不带工具**，因此读不到对话缓存（`rad/crates/agent-core/src/compaction.rs:356-385`） | 同 Radian：独立 system 加纯文本（`pi/.../compaction.ts:682-693`） | **原样历史 + 一条摘要指令**，同一个 client session（`codex/codex-rs/core/src/compact.rs:282-320`）；另有服务端 remote compact | **同样的 system、工具和历史**，最后追加一条摘要指令，缓存未过期时读缓存（[prompt-caching](https://code.claude.com/docs/en/prompt-caching)） | soft / remote（OpenAI 服务端）/ snapcompact（把历史渲染成图片）（`omp/session/compact-modes.ts:41-57`） | `[system][旧历史][摘要指令]`，共享前缀（`oc/session/compaction.ts:252-262`）；也有 native 模式 |
| 摘要结构 | Goal（引用原话）/ Constraints / Progress / **Verified facts（不要再查）** / Current work / Next step / Decisions / Critical context / Files（`compaction.rs:72-117`）；另外由 Core 确定性地带上用户原话（ADR 0018） | Goal/Progress/… 再加**确定性提取的读写文件清单**（`pi/.../compaction.ts:60-91`） | 很短的 handoff 提示（`codex/codex-rs/prompts/templates/compact/prompt.md`）；保留用户消息最多 20k tok（`compact.rs:64`） | 请求、概念、文件与代码片段、错误、待办、当前工作（[context-window](https://code.claude.com/docs/en/context-window)） | — | Objective/…/Work State/Next Move/**Relevant Files ≤15**；"能从文件里找回的就不写"（`oc/session/compaction.ts:104-143`） |
| 保留量 K | `min(20000, max(2048, B/4))` | 20 000 | 无（只留用户消息） | — | 20 000（`omp-core/compaction/compaction.ts:224`） | 15 000（`oc/session/compaction.ts:197`） |
| 轻量档："清旧工具输出" | **没有**；只在摘要失败时的确定性兜底里把工具结果截到 2 000 字符（ADR 0018 决定 3） | 没有 | 实验性的 token-budget 模式：notes + history 工具，可按 id 回读（默认关闭，`models.json` `token_budget.enabled:false`） | 文档未写（API 侧有 `clear_tool_uses`） | **有**：保护最近 40k，至少能省 20k 才动，被新读取取代的结果优先清，保温段不动（`omp-core/compaction/pruning.ts:77-82`；`omp/session/session-maintenance.ts:310,3305-3322`） | 没有（只截断进摘要的工具输出，1 250 字符，`compaction.ts:100`） |
| 压缩后补回 | system 层里的东西天然都在（AGENTS.md、memory、skills 目录） | — | 初始上下文重新插入（`compact.rs:651`） | 重读 CLAUDE.md 和 memory；**重读最近改过的最多 5 个文件**（每个 ≤5k tok）；**补回用过的 skill 正文**（每个 5k、合计 25k）；后台任务会提醒（[context-window](https://code.claude.com/docs/en/context-window)） | — | 指令单独提供，摘要里不重复（`compaction.ts:146`） |
| 工具输出进上下文前的裁剪 | `read` 默认 64 KiB；`shell` **每个流** 64 KiB（stdout + stderr 最多 128 KiB ≈ 32k tok），保留头 1/4、尾 3/4；**截掉的部分没有存档**（`rad/crates/radian-coding/src/provider.rs:63-89`；`shell.rs:577-583,1299-1308`） | 2 000 行或 50 KB，**全文存到临时文件**并给出路径（`pi/coding-agent/src/core/tools/truncate.ts:11-12`；`bash.ts:255,352`） | 10 000 tok，从中间截（`models.json` `truncation_policy`；`protocol/src/error.rs:679-680`） | 未核实（本次没查文档里的数字） | — | 2 000 行或 50 KiB，**全文存文件保留 7 天**（`oc/tool-output.ts:13-15,66-118`） |
| memory | 固定块 ≤2 000 tok，按第一条用户消息排序；之后的命中作为 System 消息追加（≤500 tok）（`rad/.../memory/injection.rs:1-24,70-79`） | — | memory 模块（未细查） | MEMORY.md 前 200 行或 25 KB（[memory](https://code.claude.com/docs/en/memory)） | memories / hindsight 等多种 | — |
| skills | system 里只放名字和描述（≤1 000 tok），正文通过 `skill` 工具读（`rad/.../skills/mod.rs:1-24`） | `<skills>` 名单 | Skills 段 | 只列描述，正文按需加载，**压缩后不再列描述** | 保护 skill 读取的结果不被剪（`pruning.ts:80`） | `skill` 工具 |
| todo / 计划复述 | 没有工具（ADR 0043 已删掉 `.radian/progress/`） | 没有 | `update_plan`（`codex/.../tools/handlers/plan.rs`） | TodoWrite / TaskCreate（`sdk-tools.d.ts`） | `todo`，并在结束前提醒（`omp/tools/todo.ts`；`agent-session-events.ts:71`） | 未核实（新的 core 目录里没看到） |

---

## 3. 各家的关键做法（只列和"快"、"省"有关的）

### Claude Code（全部来自文档）
- **压缩请求共享前缀**："摘要用一次单独的请求，system、工具、历史都和对话一样，最后加一条摘要指令作为 user 消息。缓存还热的时候，这次请求从缓存读前缀，所以中途 `/compact` 的花费只是上下文大小的一小部分。"（[prompt-caching](https://code.claude.com/docs/en/prompt-caching) "Compacting the conversation"）
- **压缩后的补水**有一张清单：重读最近改过的 5 个文件，超过 5k tok 的文件只给路径；补回 skill 正文（单个 5k、合计 25k，先丢最旧的）；git 快照重新取；提醒哪些后台任务还在跑，"免得重复启动"。（[context-window](https://code.claude.com/docs/en/context-window) "What survives compaction"）
- **TTL 分两类请求**：主对话可以用 1h，子 agent 和压缩请求默认 5m，用 `promptCacheTtl` / `subagentPromptCacheTtl` 调（同上 "Cache lifetime"）。
- **缓存可观测**：`/usage` 里显示命中率、miss 次数、缓存现在是否还热，甚至给出上一次 miss 的可能原因（同上 "Check cache performance"）。
- **嵌套 CLAUDE.md 和 path 规则按需加载**，作为消息追加；也正因为是消息，压缩时会被摘要掉（[memory](https://code.claude.com/docs/en/memory)）。
- MCP 工具定义默认延迟加载，中途连接或断开服务器都不动前缀。

### Codex
- 自动压缩阈值是窗口的 90%（`openai_models.rs:529-540`）；压缩时把原历史加上 `SUMMARIZATION_PROMPT` 发给同一个模型，同一个 client session 保持路由粘性（`compact.rs:282-300`）。压缩后只保留用户消息，最多 20k tok（`compact.rs:64,715-760`），再加摘要前缀（`prompts/templates/compact/summary_prefix.md`）。
- 工具输出统一截到 10k tok，从中间截（`models.json` 的 `truncation_policy`）。
- `prompt_cache_key` 按会话设置（`client.rs:646-657`）；websocket 模式比对前后两次请求的属性，属性没变就只发增量（`client.rs:380-456`）。
- 实验性的 **token-budget 模式**（默认关闭）：剩余约 6 144 tok 时提醒模型用 `notes` 记下进度，再调用 `new_context` 开一个新窗口；之后用 `history`/`read_item` 按 id 回读原文（`models.json` `token_budget.reminder_message_template`；`core/src/tools/handlers/new_context_window_spec.rs`、`get_context_remaining.rs`）。这是六家里唯一一种**可以按 id 找回原文**的压缩。

### opencode
- 摘要请求是 `[system][S1 + 最近][E11…E18][prompt]`，模型没按模板写就补发一次 NUDGE（`oc/session/compaction.ts:252-296`）。
- 模板很省："Relevant Files 最多 15 个"，"能从代码或文件里找回的可以不写"，"AGENTS.md 和环境信息会另外给你，摘要里不要重复"（`compaction.ts:104-146`）。
- 超长重试分 70/50/35 三档（`compaction.ts:95`），和 Radian ADR 0018 一样。
- 工具输出截断时全文存到 `tool-output/` 目录，告诉模型路径，保留 7 天（`oc/tool-output.ts:13-15,66-118`）。

### pi
- 触发点 `window − 16384`，保留 20k；摘要单独起 system（`compaction.ts:126-129,682-693`）。读过和改过的文件清单**从工具调用里确定性提取**，并在多次压缩之间累积（`compaction.ts:60-91`）。
- **缓存保温器**：在 TTL 的 90% 时补发一次请求，前提是预计能省 ≥ $0.05；空闲时继续请求的概率按实测取 0.15（`pi/coding-agent/src/core/cache-warmer.ts:15-31`）。另外支持 long retention（Anthropic 1h，OpenAI 24h）。

### omp
- **剪枝作为轻量档**：每轮先清"被新读取取代的旧读取结果"，过了阈值再剪旧工具输出（保护最近 40k，至少能省 20k 才动），之后才走摘要（`session-maintenance.ts:3305-3322`；`pruning.ts:77-82`）。离末尾不到 8k tok 的结果属于已经发出、还热的前缀，不动它，避免一次剪枝就把整段缓存重写（`session-maintenance.ts:310`；`pruning.ts:52-62`）。剪枝可以撤销（`pruning.ts:84-104`）。
- 压缩有三种方式（`compact-modes.ts:41-57`），其中 snapcompact 把历史、AGENTS.md 和大块工具结果渲染成 PNG 交给视觉模型读（`snapcompact-inline.ts:1-15`）。这一项比较特殊，Radian 不建议跟进。

### Anthropic API（文档）
- `clear_tool_uses_20250919`：默认 `trigger` 100k 输入 tok，`keep` 最近 3 次工具调用；`clear_at_least` 用来保证"值得为它打破缓存"；`exclude_tools` 列出不清的工具；默认保留工具调用参数（[context-editing](https://platform.claude.com/docs/en/build-with-claude/context-editing)）。

---

## 4. Radian 的优势

1. **前缀纪律最严，而且有测试守着**：0a/1/2/3 层一个进程只算一次，`<session>` 只按 key 重算（`rad/crates/radian-host/src/prompt.rs:236-310`），memory 块按会话固定，后来的命中作为消息追加（`injection.rs:1-24`），skills 段的输出是确定的。CC 在文档里列了 9 种会让缓存失效的操作，Radian 大多在结构上就避开了：工具定义中途不撤（ADR 0036），`/access` 切换只改执行、不改工具描述（`provider.rs:226-231`）。
2. **项目规则不会因为压缩而丢**：AGENTS.md、memory、skills 目录都在 system 里，压缩只替换消息部分。CC 的嵌套 CLAUDE.md 和 path 规则走消息，会被摘要掉（文档原话）；Radian 不存在这个问题。
3. **摘要的"接着干"结构是六家里最细的**：`Verified facts (do not re-check)` 要求写明怎么核实的、按时间排序、删的时候从最旧的删，并且有数字上限（ADR 0018）；用户原话由 Core 确定性地带过压缩，不经过模型（修订 6）。这两条直接针对 dogfood9 那种"压缩 11 次、388 次工具调用、一次编辑都没有"的空转。opencode 和 pi 都只有模型写的摘要。
4. **失败路径确定**：按原子单元切分，再过 `validate_history`；超长时 70/50/35 三档；摘要失败时有不调模型的确定性兜底（ADR 0018 决定 2、3）。
5. **上下文文件安全**：转义闭合标签，加不可信说明，用 `O_NOFOLLOW` 打开，超限先丢外层（`context.rs:9-11,136-140,166-180`）。
6. **工具描述教模型省 token**：`read` 写明"不要靠调大 `max_bytes` 去读后面的部分，那会每次重读前缀"（`read.rs:64-65`）。

## 5. Radian 的劣势和缺口

1. **TTL 太短，长等待之后缓存必然过期。** adapter 只发 `{"type":"ephemeral"}`，注释还写着"Anthropic 目前只支持 5 分钟"（`request.rs:27-29`）。这句已经过时：CC 和 pi 都在用 1h（[prompt-caching](https://code.claude.com/docs/en/prompt-caching)；`pi/ai/src/api/openai-completions.ts:1087`）。Radian 实测 `job_output` 5 次调用的中位是 361 s、最长 600 s，`shell` 最长 600 s（`rad/docs/research/2026-10-15-tool-concurrency-measurement.md:150,164`）。一次 361 s 的等待超过 300 s TTL，等完后的请求就要把整个上下文按全价重新写一遍缓存（Anthropic 的写入价是 1.25 倍）。这一条是从 TTL 机制推出来的，journal 里还没有测过。
2. **OpenAI 路由不发 `prompt_cache_key`**，Codex、pi、opencode 都发了。总报告已经列过，这里不再展开，只补一点：子 agent 应该用 `父 id:来源` 这样的 key（Codex `client.rs:651-654`）。
3. **压缩请求读不到缓存。** 摘要请求换了 system，`messages` 只有一条纯文本的 user 消息，不带工具（`compaction.rs:356-385`）。所以要摘要的那部分历史全部按未缓存价计费，prefill 也要从头算，而 CC、opencode、Codex 都能读到缓存。代价的估算（推断）：Radian 把工具结果截到 2 000 字符，纯文本可能只有原历史的 30–60%，但按 1 倍价计费；共享前缀的做法按 0.1 倍价读整段历史，花费大约是 Radian 的 1/3 到 1/6，首 token 也更快。
4. **没有轻量档。** 一过 `B` 就是整段摘要（ADR 0018:37）。omp 和 Anthropic API 都先清旧工具输出，这样能推迟甚至避免摘要。总报告提过这一点；本文补上具体参数和缓存护栏，见 §6.4。
5. **压缩后不补读文件、不补回 skill。** 摘要里的 `Files read and modified` 由模型自己写，没有确定性提取（pi 有）；压缩后不重读最近改过的文件（CC 有）。`skill` 工具读进来的正文如果不在最后的 K 里，就会被摘要掉，压缩后模型只能再读一次。这几条都会造成"压缩之后重查"。
6. **截掉的工具输出找不回来。** 前台 `shell` 的输出超过 64 KiB 后丢弃中段，没有存档（`shell.rs:1299-1308`）；模型想看被截掉的部分，只能把命令重跑一遍，而有些命令要 72–600 s。pi 和 opencode 都把全文存成文件并给出路径。另外 Radian 的默认值也偏大：两个流合起来最多约 32k tok，Codex 是 10k tok。
7. **固定前缀偏重。** 实测 12 个编码工具的定义 14.3 KB，`<runtime>` 层又把工具名单和用途写了一遍（`tools.rs:99-160`，约 1.3 KB，按源码估算）。光 `shell` 加 `job_output` 就 6.1 KB。
8. **子目录的 AGENTS.md 永远读不到**：只读工作区到仓库根这一条路径（ADR 0036 C2）。如果会话从仓库根启动，`crates/foo/AGENTS.md` 就不会被加载。CC 会在读到该子目录的文件时加载。
9. **触发点不能设置**：1M 窗口的模型要到约 98 万 tok 才压缩，每轮都要读一遍这么大的缓存（推断：在 $0.30/MTok 的缓存价下，每轮约 $0.29）。只能用 `--context-window` 变相调小窗口。CC 有 `/autocompact`，omp 有 `thresholdPercent`。
10. 没有 todo 工具，长任务里的计划只能靠摘要的 `Pending`/`Next step` 带过去。

---

## 6. 改进方案（按收益/成本排序）

### 6.1 Anthropic 支持 1h TTL，等待超过 TTL 时保温（收益高，成本低）
- **做什么**：在 `CacheMode` 里加一个 `Auto1h` 或 `ttl` 字段，把断点写成 `{"type":"ephemeral","ttl":"1h"}`，先只用在主对话上（照 CC 的分法，子 agent 和摘要请求仍用 5m）。另一个做法：在宿主层里，如果 `job_output wait:true` 或长 `shell` 预计会超过 TTL 的 90%，就像 pi 那样补发一次最小请求保温（`cache-warmer.ts:28-31`，只在预计能省 ≥ $0.05 时才发）。
- **为什么**：长等待的中位数已经超过 5 分钟（§5.1），之后的每次请求都会整段重写。1h TTL 的写入价更高，但只要有一次长等待就能赚回来（CC 文档对这个取舍的说明）。
- **成本**：adapter 改几十行，再改 wire notes 和两套契约测试（契约规则：不能编造 provider 不发的值；`ttl` 是 Anthropic 有文档的字段）。不推翻任何 ADR；ADR 0036 只规定了断点放在哪里。需要给 Anthropic adapter 的配置补一条说明，可能还要动 ADR 0026 的路由配置。
- **验证**：用 journal 里的 usage 算"长等待之后那次请求的 `cache_read/(cache_read+input)`"，对比改动前后。变异测试：TTL 不写进请求时，测试应该失败。

### 6.2 OpenAI Responses 和兼容路由发 `prompt_cache_key`（收益中，成本很低）
- **做什么**：主对话用 session id；`subagent` 用 `"{父 session}:subagent"`（Codex `client.rs:646-657`；opencode 的 affinity）。
- **成本**：几行代码，加上契约测试的两条声明。和第 5 区（provider 层）有重叠，由那边统一落地。

### 6.3 摘要请求共享对话前缀（收益高，成本中，需要修订 ADR 0018）
- **做什么**：把 `summary_request` 改成 `system = 对话的 system`、`tools = 对话的工具`、`messages = 原样的 to_summarize（加上 previous summary）+ 一条 user 摘要指令`。在支持的 provider 上设 `tool_choice: none`；不支持的照 opencode，写明"不要调用工具"，模型没按模板写就补发一次 NUDGE（`oc/session/compaction.ts:274-290`）。缓存已冷（离上次请求超过 TTL）时退回现在的纯文本路径，因为那时纯文本更省。
- **为什么**：§5.3 估算花费约为现在的 1/3 到 1/6，摘要的 prefill 也会快很多。CC 文档专门把这一点当成 `/compact` 便宜的原因。另外模型能看到完整的工具结果（不再被截到 2 000 字符），`Verified facts` 的依据会更准。
- **成本**：ADR 0018 写了"摘要器只看纯文本，所以造不出坏的配对"。新路径的摘要输出只取文本，不进 transcript，配对问题依然不存在，但这条决定的理由要改，**需要新的修订**。另外：摘要请求的 `reasoning`/`thinking` 档位（修订 8）在部分模型上是缓存 key 的一部分（CC 文档说大多数模型的 effort 是），所以共享前缀时要么用对话的档位，要么接受缓存 miss。这一点要在修订里明确。
- **验证**：mock 验证请求的前缀和上一次对话请求逐字节一致；真实会话里看摘要请求的 `cache_read` 占比；`compaction_flow` 的 16 个测试继续通过。

### 6.4 轻量档：先清旧工具输出，再做摘要（收益高，成本中，需要新 ADR）
- **做什么**：在 `T > θ·B`（比如 θ = 0.8）时，把"保护窗口"以外的旧工具结果换成占位，例如 `[output elided: <tool> <args 摘要>; re-run or read <path>]`，保留工具调用参数。参数照 omp：保护最近 40k、至少省 20k 才动、被同一路径的新读取取代的结果优先清、离末尾不到 8k 的不动（`pruning.ts:77-82`；`session-maintenance.ts:310`）。也可以照 Anthropic 的 `keep: 3`、`clear_at_least`。实现上可以直接复用确定性兜底里已有的 `truncate_kept_tool_results`（`compaction.rs:572`）。
- **为什么**：工具结果是上下文里最大的一块（单次最多 32k tok）。清掉旧的就能推迟整段摘要，摘要也就不会丢掉 Goal 和 Verified facts 之外的细节。每次清理会重写一次缓存，所以要有 `clear_at_least` 那样的门槛。
- **成本**：这会改变已经发出的历史，和 ADR 0036 的缓存前提冲突，又要走 ADR 0019 的 transcript 替换提交，并且通过 journal-selfcheck。**需要新 ADR**，说明清理也是一种 transcript 替换、按什么门槛触发、怎样和 0018 的阈值配合。
- **验证**：在长会话回放里统计压缩次数和总输入 token；变异测试：保护窗口失效、门槛失效、清掉了被保护的工具，这几种情况测试都应该失败。

### 6.5 压缩后确定性补水（收益中高，成本低到中，需要修订 ADR 0018）
- **做什么**：(a) 照 pi，从工具调用里确定性地提取读过和改过的文件清单，追加到摘要里，并在多次压缩之间累积（`pi/.../compaction.ts:60-91`），不再让模型去写 `Files read and modified`。(b) 照 CC，压缩提交之后追加一条 System 消息，带上最近改过的 ≤3–5 个文件的当前内容（每个 ≤5k tok，超过就只给路径），以及本会话用 `skill` 读过的正文（单个 5k、合计 25k）。(c) 照 CC，提醒还在跑的后台 job（Radian 已经有 job 注册表，ADR 0037）。
- **为什么**：这几样东西正是压缩后最常被重新读取的。CC 把它们写成了固定规则；opencode 也在摘要里说"这些会另外给你"。这样能减少压缩之后头几轮的重复 `read` 和 `job_list`。
- **成本**：(a)、(c) 很小。(b) 是 append-only 的 System 消息，符合 ADR 0036 的"中途变化以消息追加"，但放在压缩之后的固定位置，要写进 ADR 0018，作为摘要结构的修订。
- **验证**：回放压缩之后的前 5 轮，统计 `read` 同一路径和 `job_list` 的次数。

### 6.6 截断的工具输出存档，并下调默认上限（收益中，成本中，碰到 ADR 0037、0039）
- **做什么**：前台 `shell` 一旦截断，就把全文写进会话目录里的 job 式输出文件，在截断标记里给出可以用 `job_output`（`from_line`/`pattern`）读的句柄。ADR 0039 不允许模型直接读这个路径，所以只能通过句柄读。在这个基础上，把每个流的默认值从 64 KiB 降到 16–24 KiB（Codex 10k tok；pi 和 opencode 50 KB）。
- **为什么**：避免为了看日志中段而把 72–600 s 的命令重跑一遍（这一条是速度收益）；每次截断都能少几千到两万多 tok 的上下文。
- **成本**：job 的输出环和文件已经有了（`jobs.rs`），主要工作是把前台命令也接进去，以及修订 ADR 0037 的工具描述。
- **验证**：变异测试：截断后句柄读不到中段时，测试应该失败；评测集里统计"同一条命令重跑"的次数。

### 6.7 给固定前缀瘦身（收益中，成本低，需要修订 ADR 0036 D1）
- **做什么**：从 `<runtime>` 里去掉工具名单和用途，只保留档位和沙箱事实，因为工具定义里已经写过一遍了，大约省 1.3 KB；把 `shell` 的描述（1 900 B）和 `job_output`（825 B）里重复的话收短。
- **为什么**：这一块每个请求都要带，虽然缓存后只按 0.1 倍计费，但冷启动、子 agent 第一次请求和换模型时都要全价付。
- **成本**：ADR 0036 D1 把工具名单写进了 0a 层，需要一条修订。测试里断言了 `- read: read a file`，要跟着改。
- **验证**：用 §2 的方法重新测一遍字节数，再用评测集确认成功率没有下降。

### 6.8 子目录 AGENTS.md 按需加载（收益中，成本中，需要修订 ADR 0036 C2）
- **做什么**：结构化工具第一次碰到工作区下面某个子目录时，如果那里有 AGENTS.md，就把它作为一条 System 消息追加，转义规则和不可信说明照旧，每个目录只加一次（照 CC；dsh 也是这样，见提示词调研 §6）。
- **成本和取舍**：消息会被压缩摘要掉（CC 文档说明了这个代价），所以压缩后要再追加一次，可以和 6.5 一起做。

### 6.9 可配置的压缩触发点（收益在 1M 窗口上明显，成本低）
- **做什么**：加一个 `[compaction] threshold`，支持 token 数或百分比（照 omp 和 CC `/autocompact`），用来调小 `B`，但不改 `R` 的计算。
- **碰到**：ADR 0018 的 `B` 公式要补一个可选的上限，需要修订；还要改 ADR 0008 的配置 schema。

### 6.10 计划复述（收益未知，先不做）
- Codex、CC、omp 都有 todo 或 plan 工具，omp 还会在结束前提醒没做完的 todo。Radian 的摘要已经用 `Current work`/`Next step` 带过压缩。建议等评测集建好后再决定，免得给前缀加一个工具定义。

---

## 7. 未核实与说明

- Radian 的 memory、ask、skill、subagent 四类工具的定义大小是**按源码压缩空白后估算的**（`tools.rs`、`memory/tools.rs:438-565` 等），不是序列化后实测；12 个编码工具的 14 297 B 是在 scratchpad 里用临时 crate 调 `CodingTools::open_session().tool_specs()` 序列化后实测的（没有改仓库）。
- "长等待之后缓存过期"是从 5 分钟 TTL 推出来的，没有拿 journal 的 `cache_read` 核对；6.1 的验证就是这个测量。
- §5.3 的成本比例是推断，取决于工具结果在历史里占多少。
- Claude Code 的条目全部来自 2026-10-18 抓取的官方文档，没有看二进制。CC 的工具输出上限本次没有核实。
- opencode 新 core 里有没有 todo 工具没有核实；Codex 的 token-budget 模式默认关闭，没看到它的使用效果。
- omp 的阈值默认值：`thresholdPercent:-1` 表示使用 reserve 公式（`omp-core/compaction/compaction.ts:217-228,413-418`），reserve 的具体默认值没有展开查。

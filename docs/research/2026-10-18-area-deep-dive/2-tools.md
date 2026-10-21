# 深挖 2：工具层（Tools）

日期：2026-10-18（当天按讨论补充了 P3 编辑后诊断的分层方案和 P6 的 rg 结论）· 范围：默认工具集、工具定义大小、编辑、读/搜、shell、报错、编辑后诊断、子 agent、按需加载、结果格式。
对手版本：Codex `openai/codex` 提交 `8c07694`；opencode `packages/core`（这一版的工具已从 `packages/opencode/src/tool` 搬到 `packages/core/src/tool/plugin`，旧路径不存在）；pi-mono `packages/coding-agent`；omp 18.12.1（`npm-_oh-my-pi_pi-coding-agent/package/src`）；Claude Code 2.1.302（只用 `sdk-tools.d.ts` 和官方文档 https://code.claude.com/docs/en/tools-reference ）。下文 `rad/` = `<repo>/crates`，`comp/` = 对手源码根目录。

## 1. 一句话结论

Radian 的工具层在**安全和报错质量**上领先（每个参数错误一次列全、`patch` 找不到时直接给最接近的行），但**定义最重**：17 个工具约 20.0 KB，约 5.0k token，是 pi 默认集的 7 倍、Codex 的 4 倍。它还缺三样东西，这三样直接决定编辑次数、读的次数和等待时间：一次调用改多处（multi-edit）、编辑后自动诊断（LSP 或检查命令）、shell 被截断的输出没有落盘。补上这三样，再把定义瘦到 12–13 KB，收益最大，成本也最小。

## 2. 对比表

| 维度 | Radian | pi | Codex | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 默认工具数 | 17（TUI 全挂：coding 11 + memory 3 + ask + skill + subagent；`rad/radian-host/src/tools.rs:99-183`） | 4：read/bash/edit/write；grep/find/ls 可选（`pi-mono/.../tools/index.ts:164-171`） | 7 个直接可见：exec_command、write_stdin、apply_patch、update_plan、view_image、sleep、tool_search；协作工具延迟加载（`core/src/tools/spec_plan.rs:1286-1440, ~1540`） | 很多；Linux/macOS 上默认**不给** Glob/Grep，改由 Bash 里内嵌的 bfs/ugrep 搜索（文档 Glob tool behavior） | 13 个 essential，其余“discoverable”（`tools/essential-tools.ts:24-38`） | 12 个原生工具 + CodeMode `execute`；GPT 用 `patch` 代替 edit/write（`tool/plugin/patch.ts:296-305`） |
| 工具定义大小（实测/估算，见 §2.1） | **20,038 B ≈ 5.0k tok**（实测） | 2,720 B ≈ 0.7k（4 个）；7 个是 4,774 B | ≈ 5,077 B ≈ 1.3k（估算） | 无法从允许的来源测；二手资料说曾是 10–14k token（Composio，见前一份总报告） | ≥ 14.3 KB 描述模板原文，未算 schema（下限） | ≈ 13,050 B ≈ 3.3k（估算，含 patch；Claude 路线约 11.7 KB） |
| 编辑格式 | `patch{path,old,new,replace_all}`，**每次只改一处**（`rad/radian-coding/src/patch.rs:13-21`） | `edit{path,edits[]}`，**一次可改多处**（`edit.ts:21-41`） | `apply_patch` 自由格式 + Lark 语法，多文件、多 hunk（`apply_patch_spec.rs`） | `Edit{old_string,new_string,replace_all}`，一次一处（`sdk-tools.d.ts:864-881`） | 默认 `hashline`：`[PATH#TAG]` + 行号操作 PUT/CUT（`edit/settings.ts:40`, `edit/hashline-compact.md`） | 非 GPT：`edit` 一次一处；GPT：`patch`（apply_patch 格式，`tool/patch.txt`） |
| 匹配策略 | 精确 → 排版字符归一 → 逐行忽略行尾空白；**不忽略缩进**（`patch.rs:284-317`） | 精确 → NFKC + 引号/破折号 + trimEnd（`edit-diff.ts:34-40, 207-245`） | 精确 → rstrip → **两端 trim（容缩进）** → Unicode 归一，用 `@@` 上下文定位（`apply-patch/src/seek_sequence.rs:35-72`） | 只做精确匹配，“不用正则也不模糊”（文档 Edit tool behavior） | 默认按行号 + 快照 tag；替换模式下模糊阈值 0.95（`edit/settings.ts:82-84`） | 精确 → Unicode → 行尾空白，和 Radian 几乎一样（`tool/plugin/edit.ts:129-138`） |
| 先读后改 / 文件变动检测 | 无 | 无 | 无（靠上下文） | 有：没读过要求先读；文件被改过，仍按当前内容精确匹配，并提示“文件有别的改动”（文档 Edit） | 有快照 tag：tag 不对就拒绝（hashline-compact.md） | 无 |
| read 默认上限 | 64 KiB，**没有行数上限**；有 offset/limit/next_offset（`provider.rs:67`, `read.rs:54-73`） | 2000 行或 50 KB（`truncate.ts:11-12`） | 没有 read 工具，用 shell 的 `sed`/`rg` | 25k token；单文件超过 256 KB 拒绝，可加 `allow_large`（文档 Read） | 路径带范围后缀；**读代码文件时默认只给声明摘要**（`prompts/tools/read.md`） | 2000 行，也能读目录（`tool/plugin/read.ts:17-25`） |
| grep | 进程内 regex，默认 200 条匹配，每个文件 50 条，每行 8 KiB；**没有 files_with_matches/count 模式，也没有总字节上限**（`provider.rs:73-75`, `grep.rs:22`） | 调 rg，默认 100 条，50 KB，每行 500 字符（`grep.ts:41`, `truncate.ts:13`） | 用 shell 跑 `rg` | ripgrep，默认 `files_with_matches`，`head_limit` 250（`sdk-tools.d.ts:924-985`） | 有 grep 和 ast-grep | 调 rg，结果统一截到 2000 行/50 KiB |
| shell 进程模型 | 每次新进程 `/bin/sh -c`，cwd 和环境变量都不保留（`shell.rs:420`） | 每次新进程（`bash.ts:112`） | **PTY 会话可以续**：`exec_command` 等 10 s 后返回会话 id，用 `write_stdin` 继续（`unified_exec/mod.rs:68-77`） | 每次新进程；cwd 在项目目录内会保留，环境变量不保留（文档 What persists） | **持久 shell**（`prompts/tools/bash.md:1`） | 每次新进程，`workdir` 参数 |
| 超时 / 转后台 | 不给超时时等 120 s 后原地转后台；给了超时则默认 60 s，最长 600 s（`provider.rs:84-88`, ADR 0037） | 默认没有超时（`bash.ts:42`） | 等待默认 10 s（250–30000 ms），之后进程照跑 | 默认 2 min、最长 10 min，到时转后台（文档 Timeout） | 60 s 后自动转后台（`exec/settings.ts:263-266`） | 默认 2 min，有后台模式和完成通知（`tool/plugin/shell.ts:5-7`） |
| shell 输出上限 | 每个流 64 KiB，stdout+stderr 加起来最多 128 KiB ≈ 32k token；保留头尾，**中间直接丢掉，不落盘**（`shell.rs:1298-1311`） | 尾部 2000 行或 50 KB，**全文存到临时文件**（`bash.ts:348-352`） | 默认 10k token（`unified_exec/mod.rs:74`） | 成功约 30k 字符，超过就给文件路径 + 预览；失败约 10k 字符，取头尾（文档 Output limits） | 截断，全文在 `artifact://<id>`（bash.md） | 2000 行/50 KiB，全文存到文件，保留 7 天（`core/src/tool-output.ts:13-15`） |
| 编辑后诊断 | 无；但 agent-core 已有 `after_tool_call` hook 可以挂（`rad/agent-core/src/hooks.rs:89`） | 不内建，靠扩展 | 无，靠模型自己跑测试 | 两条路，都不是内建 tool：装 LSP 插件后每次编辑自动报类型错误（文档 LSP）；用户配 `PostToolUse` hook 跑 lint，输出回给模型（文档 hooks） | 有：LSP writethrough 和延迟诊断（`edit/index.ts:29-33`） | 这一版只有 TODO：“LSP runtime 有了再加”（`tool/plugin/edit.ts:205`）；旧版据说做过 LSP 诊断，未核实 |
| 子 agent | `subagent{agent?,task}`：阻塞，深度 1，`explore`/`general`，结论截到 4000 字符；**不能选模型，没有后台，不能续接**（`rad/radian-host/src/subagent.rs:60, 178-205`） | 只有扩展示例 | `spawn_agent`/`send_input`/`wait_agent`…，默认延迟加载 | `Agent`：**默认后台**，有 model 和 effort 参数，可以用 `SendMessage` 续接，可以开 worktree 隔离（`sdk-tools.d.ts:772-813`；文档） | `task`，带角色和模型 | `subagent`：有 model、`sessionID` 续接、`background`（`tool/plugin/subagent.ts:29-50`） |
| 按需加载工具 | **无**：MCP 工具全部进入每次请求 | 无 | MCP/apps 默认延迟加载，用 BM25 `tool_search`（`spec_plan.rs:265-273`, `tool_search_spec.rs`） | 默认开 `ToolSearch`（文档 mcp） | `xd://` 设备 + BM25 搜索，`tools.xdev` 默认开（`tools/settings.ts:913-915`；agent-core `types.ts:1046-1054`） | CodeMode：非原生工具只能在 `execute` 的脚本里通过 `search()` 找到再调用（`core/src/codemode/tool.ts:62-69`） |
| 结果格式 | 纯文本；`structured` 不发给模型（`read.rs:315-317` 的注释） | 文本 | 文本 | 文本 | 文本 + 内部 URL | 文本；CodeMode 的结果是 JS 值 |

### 2.1 工具定义大小是怎么算的

- **计法**：每个工具序列化成紧凑 JSON `{"name","description","input_schema"}`（Anthropic 的形状；Codex 用 Responses 的 `{"type","name","description","parameters"}`，自由格式工具是 `format.definition`），按 UTF-8 数字节数，token ≈ 字节 / 4。每家都按同一口径算。provider 外层包装（每个工具约 30 B）没算。
- **Radian（实测）**：脚本 `scratchpad/tools2/radian_specs.py` 解析 `rad/radian-coding/src/*.rs` 和 `rad/radian-host/src/{memory/tools.rs,skills/tools.rs,subagent.rs}` 里每个 `ToolSpec` 的 `name`/`description`/`json!` schema。常量取源码值（如 `MAX_QUESTIONS=8`）。`shell` 取 `full` 档的描述（ADR 0043 起的缺省）。被宿主剥掉的别名 `git_status`、`memory_get` 不算（`rad/radian-host/src/model.rs:189-196`）。结果：

  | 工具 | 字节 | 工具 | 字节 |
  |---|---|---|---|
  | shell | 3,723 | job_output | 2,275 |
  | ask_user | 1,988 | read | 1,578 |
  | grep | 1,514 | patch | 1,191 |
  | memory_save | 1,042 | glob | 1,007 |
  | memory_search | 926 | subagent | 883 |
  | git_diff | 729 | skill | 645 |
  | list | 635 | memory_revise | 583 |
  | write | 500 | job_kill | 485 |
  | job_list | 334 | **合计 17 个** | **20,038 ≈ 5.0k tok** |

  只算 coding 那 11 个是 13,971 B。描述一共 8,442 B，schema 是另外约 11.6 KB。schema 里很多参数说明重复了描述里的话，例如 `shell` 的 `background`/`timeout_secs` 把后台规则又讲了一遍（`shell.rs:68-114`）。系统提示词的 `<runtime>` 层还会再列一次工具名和一句话用途（`tools.rs:99-183`，约 1.3 KB），这部分没算进上表。
- **pi（转写，精确到字段）**：按 `tools/*.ts` 的 TypeBox 定义手工转成 JSON（`scratchpad/tools2/pi_specs.py`）。read 655、bash 514、edit 1,150、write 401，合计 2,720 B。
- **opencode（估算）**：工具描述从源码自动抽取；每个 Input 字段估成 `{type:string, description}`（`scratchpad/tools2/oc_specs.py`），所以会少算 enum 和数字约束。合计 13,050 B。其中 subagent 1,887 B，而且运行时还会把可用 agent 清单追加到描述里（`subagent.ts:297-305`），实际更大。
- **Codex（估算）**：按 `shell_spec.rs`、`plan_spec.rs`、`view_image_spec.rs`、`sleep.rs`、`tool_search_spec.rs` 转写；apply_patch 计入 578 B 的 Lark 语法。合计 5,077 B。另外，GPT-5.x 的基础提示词里还有一整节 apply_patch 说明（`core/gpt_5_2_prompt.md:254` 起），没算进来。
- **omp（下限）**：13 个 essential 工具的描述模板原文（`prompts/tools/*.md` + `edit/hashline-compact.md`）共 14,282 B。没算 schema；模板里的条件分支会让实际值上下浮动。
- **Claude Code**：`sdk-tools.d.ts` 只有输入类型，没有描述，测不了。**未核实。**
- **注意**：工具定义位于可缓存的前缀里（ADR 0036：系统提示词每个进程只组装一次），所以命中缓存后每轮只按约 0.1 倍计费。它真正的代价在三处：占上下文窗口、第一轮的 prefill 延迟、没命中缓存时的整价。

## 3. 每家的关键做法（只列和快、省有关的）

**pi**
- 只有 4 个工具，定义 2.7 KB。搜索靠 bash（`index.ts:164-171`）。
- `edit` 一次可以改多处，要求各处互不重叠，都对原文件匹配；工具指引明确写“同一文件多处改动用一次调用”（`edit.ts:43-50`）。
- 找不到时只说“必须精确匹配”，不给附近的内容（`edit-diff.ts:253-262`）。这一点比 Radian 弱。
- 工具调用默认并行执行（`packages/agent/src/agent.ts:253`）。同一个文件的写操作排队，不同文件并行（`file-mutation-queue.ts`）。
- bash 被截断时全文存到临时文件，并告诉模型路径和行号范围（`bash.ts:348-352`）。

**Codex**
- 没有 read、grep 或 list 工具，只有 `exec_command` 和 `apply_patch`，读和搜靠 shell 里的 `rg`/`sed`。
- `exec_command` 默认等 10 s，命令还在跑就返回一个会话 id，模型用 `write_stdin` 轮询或交互（`shell_spec.rs:27-31`, `unified_exec/mod.rs:68-77`）。同时最多 64 个进程。
- apply_patch 用自由格式加 Lark 语法约束，模型不用把补丁转义成 JSON 字符串（`apply_patch_spec.rs:9-28`）。一次调用能改多个文件、多个 hunk。
- 匹配四档，第三档两端 trim，能容忍缩进偏差（`seek_sequence.rs:54-58`）。
- 提示词里写着“apply_patch 之后不要重读文件，失败了工具会报错”（`gpt_5_2_prompt.md:130`）。这一句直接减少无效的读。
- 模型支持搜索时，MCP 和协作工具默认 deferred，不进入直接可见的工具列表（`spec_plan.rs:265-273`）。

**Claude Code**（来源：文档）
- Linux/macOS 上默认去掉 Glob/Grep，用 Bash 里内嵌的 `bfs`/`ugrep` 搜。这样工具更少，搜索行为也和模型训练时见过的 shell 习惯一致。
- 输出分“成功/失败”两档：成功约 30k 字符，超过就给文件路径 + 2,000 字符预览；失败约 10k 字符，取头尾。`grep` 等命令退出码 1 算成功。
- 前台命令到超时不杀，转后台；`sleep` 开头的命令例外。
- Edit 要求先读；文件被改过时，按当前内容精确匹配，并提示“文件还有别的改动，依赖周边内容前请重读”。用 `cat`/`sed -n`/`rg` 看过一个文件也算读过。
- 有 LSP 插件时，每次编辑后自动报类型错误和警告，模型“不用单独跑构建”。
- 子 agent 默认后台运行，可以选 model 和 effort，用 `SendMessage` 续接。

**omp**
- 13 个 essential 工具，其余 discoverable：挂成 `xd://` 设备，或用 BM25 搜到再调用。这样“它们的 schema 不进每次请求”（agent-core `types.ts:1046-1054`）。
- `read` 读代码文件时默认只给声明摘要（tree-sitter），“只重读 footer 点名省略的部分”（`prompts/tools/read.md`）。
- 一次完整读会让之前对同一文件的局部读结果从上下文里剪掉（`tools/read-supersede.ts:1-7`）。
- 默认 hashline 编辑：用行号加快照 tag 定位，不需要复述旧文本，所以输出 token 少；tag 过期就拒绝执行。
- 编辑后有 LSP 诊断，持久 shell，60 s 自动转后台，命令结果自动作为后续消息送回。
- 还有一个“把模型写成纯文本的编辑恢复成工具调用”的开关，默认开（`edit/settings.ts:105-107`）。

**opencode**
- 编辑匹配三层，和 Radian 一样（`edit.ts:129-138`）。描述里点明“不要把 `1: ` 行号前缀带进 oldString”。
- 所有工具的结果都套一层统一截断（2000 行/50 KiB），全文落盘（`core/src/tool-output.ts`）。shell 自己保存全部输出，截断时返回尾部和全文路径（`tool/AGENTS.md` 的 Output 段）。
- 按模型选编辑工具：GPT 用 apply_patch，其他模型用 edit+write，两套不会同时挂上（`patch.ts:296-305`）。
- CodeMode：非原生工具和 MCP 默认只在 `execute` 的 JS 运行时里可见，同一个脚本可以用 `Promise.allSettled` 并发调用多个工具（`codemode/tool.ts:62-69`）。
- 后台命令的结果自带一段强指令“不要轮询，没事做就结束回复”（`shell.ts:7`）。
- `read` 会顺带加载所读目录下的 AGENTS.md（`read.ts:15-16`）。

## 4. Radian 的优势

1. **报错可操作，一次就能改对**：参数错误一次列出全部问题，回显参数，以固定的一句指令结尾（`rad/radian-coding/src/args.rs:1-10, 41-55`）。`patch` 找不到时给最接近的区域和行号，“不必再 read”（`patch.rs:319-331`）。`read` 文件不存在时给 `Did you mean …`（`read.rs:120-142`）。grep 的 `context` 超上限时截断而不是拒绝（`grep.rs:105-110` 注释：此前 7 次调用都因此多花了一轮）。五家里只有 Radian 把这几项都做全了。
2. **截断提示精确**：`read` 截断时给 `next_offset`，只丢掉不完整的行（`read.rs:314-325`）；shell 截断时说明丢了多少字节、总共多少（`shell.rs:1298-1311`）。
3. **编辑匹配和 opencode 持平，比 Claude Code 和 pi 宽**，而且每一层“只定位、不改写”，CRLF 文件改完仍是 CRLF（`patch.rs:52-68`）。
4. **后台任务设计完整**：转后台时进程原地保留、不重跑；有名额上限；完成时通知；按会话 kill（ADR 0037 与修订 1/2）。和 Claude Code、omp 同一档。
5. **路径边界硬**：fd 相对打开，每一级 `O_NOFOLLOW`（ADR 0039）。这是安全上的优势，不影响速度。

## 5. Radian 的劣势和缺口

1. **定义最重**：17 个工具 5.0k token，`shell`+`job_*` 就占 6.8 KB。ADR 0043 已经从 32 个砍到 17 个，但每个描述都写得很长。
2. **没有 multi-edit**：一个文件改 N 处就要 N 次 `patch` 调用、N 轮模型往返。journal 实测：`t4` 有 70 次 patch、64 次 read（`docs/research/2026-10-15-tool-concurrency-measurement.md` §3），而 93% 的回复只调一个工具（同文 §1）。pi 和 Codex 都能一次改多处。
3. **匹配不容忍缩进偏差**：第 3 层只忽略行尾空白（`patch.rs:442` 起）。Codex 的第三档能容忍（前一份总报告已提，这里不重复理由）。
4. **歧义报错只给次数，不给位置**：`patch_ambiguous` 只说“出现 N 次”（`patch.rs:104-115`）。模型只能再读一遍，或者盲目加上下文。
5. **编辑后没有诊断**：改完要再跑一次 `shell cargo check`，又是一轮往返。Claude Code 和 omp 都会自动反馈。
6. **shell 被截断的中间部分直接丢掉，不落盘**：64 KiB/流，最多约 32k token，一次就塞进上下文；想看被丢的部分只能**重跑命令**（描述原话：“a truncated run can be repeated”，`provider.rs:80-82`）。重跑构建或测试正是最耗时的部分。pi、opencode、Claude Code、omp 都把全文存到文件。而且 Radian 的上限比别家大：pi/opencode 50 KB，Claude Code 30k 字符，Codex 10k token。
7. **grep 缺 files_with_matches/count 模式，也没有总字节上限**：最坏情况是 200 条匹配 × 8 KiB/行（`grep.rs:22`），再加上下文 30 行，会远超 64 KiB。“哪些文件里有它”这种问题只能拿到全部匹配行。Claude Code 默认就是 `files_with_matches`。另外，测量用的三个会话 479 次工具调用里 `grep` 一次都没出现（`docs/research/2026-10-15-tool-concurrency-measurement.md` 的合计表），模型可能是在 `shell` 里直接跑 `rg`（推断，没有逐条核对 shell 命令）。
8. **没有按需加载**：MCP 工具 schema 全部进入每次请求。Codex、Claude Code、omp、opencode 都有延迟加载或搜索机制。
9. **subagent 能力窄**：不能选模型（ADR 0043 1a 第 2 条说的“explore 可配更便宜的模型”只在用户级配置里，模型不能按任务选）；没有后台；不能续接，追问只能重开一个子会话、重新读一遍。
10. **等待时间**：不给超时的命令要阻塞 120 s 才转后台（omp 60 s，Codex 10 s）。实测 shell 的 p90 是 72 s，最长的都卡在 600 s 上限（measurement §3）。另外，工具描述说 read/grep “run in parallel”，但它们的 `async fn` 里全是同步 IO（radian-coding 里没有 `spawn_blocking`），而 Core 在同一个任务里用 `FuturesUnordered` 轮询（`rad/agent-core/src/turn.rs:2159`）。所以同一批里的几个 Shared 文件工具**很可能是串行执行的**（推断，未测）。按实测每个约 10 ms，影响很小，但在大仓库上跑 grep 时会更明显。
11. **小缺口**：read 不能读图片（遇到二进制直接拒绝）；没有“先读后改”或文件变动检测；`/bin/sh` 而不是用户的 shell；默认提示词里没有“独立的调用放在同一条回复里”（`rad/radian-host/prompts/default.md` 里搜不到 parallel，属于提示词区域，这里只记一笔）。

## 6. 改进方案（按收益/成本排序）

### P1. `patch` 支持一次多处：`edits: [{old,new,replace_all?}]`（收益高，成本低）
- **做什么**：保留 `path,old,new` 单处形式，加一个可选的 `edits[]`。每一处都对**原文件**定位，复用现有三层 `locate`；有重叠就拒绝；全部成功才写回，原子性和现在一样。报错时写明是第几处（`edits[2]`），并给那一处最接近的区域。
- **为什么更快**：N 处改动从 N 轮往返变成 1 轮，每轮都要 prefill 整个上下文。pi 的工具指引就要求“同文件多处用一次调用”（`edit.ts:43-50`）。Codex 的 apply_patch 也天然支持多 hunk。
- **成本**：`patch.rs` 约 100 行，加测试。schema 增加约 300 B。
- **ADR**：0028 修订 5 决定 5 的 hunks 已经按“每个替换范围一个 hunk”设计，直接兼容；不推翻任何决定。工具 schema 是公共面，按 AGENTS.md 要在 README 的工具表里写明。
- **验证**：变异测试——重叠检测去掉、以及改成按增量文本匹配，都要有测试失败。在 dogfood 评测集上比较每个任务的 patch 调用数和轮数。

### P2. shell 截断时全文落盘，默认上限降到约 30–32 KiB（收益高，成本低）
- **做什么**：前台 shell 的 stdout/stderr 按流写进会话自己的 jobs 目录（ADR 0039 决定 1 已有这个目录，0700）。截断时结果里给出路径，并说明“用 `read`/`grep`… 看全文”。这里有一个限制：沙箱档下，这个目录在模型的沙箱内不可读，要么由 `job_output` 那套文件读取接口提供，要么把 `read` 的只读根扩展到这个目录。`default_shell_output_bytes` 从 64 KiB 降到约 16 KiB/流，并按“失败时多给尾部”的做法分配。
- **为什么更快更省**：不用重跑构建或测试就能看到被丢掉的部分（今天只能重跑，`provider.rs:80-82`）。每次 shell 结果的上限从约 32k token 降到约 8k，别家的上限都在这一量级（pi/opencode 50 KB，Claude Code 30k 字符，Codex 10k token）。
- **成本**：jobs 已经有“写文件 + 环形缓冲”的实现（`jobs.rs`，`OUTPUT_HINT`），前台可以复用。中等偏低。
- **ADR**：触及 0037 决定 5（“结果太大只带预览加路径”，这次扩展到前台）和 0039 决定 1（文件放在哪）。按 0037 修订的形式补一条，不推翻已有决定。
- **验证**：一条输出 1 MB 的命令，检查返回不超过上限、路径可以分页读完、内容和原输出一致。在评测集上看重跑同一命令的次数。

### P3. 编辑后诊断：内置语法检查 + 项目配置的检查命令（收益高，成本中）
- **形态**：不做成让模型调用的新 tool，而是 `patch`/`write` 成功后由宿主自动跑，结果附在编辑结果里。做成新 tool 的话，模型还得记得去调用，省不掉那一轮往返。宿主侧挂在 agent-core 已有的 `after_tool_call` hook 上（`rad/agent-core/src/hooks.rs:89`），不用改 Core。
- **第 1 层，内置语法检查（零配置，默认开）**：用 tree-sitter（C 核心，有官方 Rust 绑定，每种语言一个语法包）把改过的文件解析一遍，树里出现 `ERROR`/`MISSING` 节点就报行号。毫秒级，不起外部进程，也**不执行仓库里的任何代码**，所以不碰项目信任（ADR 0031）。能当场抓住括号不配对、少写 `end`、缩进弄坏代码块这类模型常犯的错误。Aider 的 linter 也是先用 tree-sitter 查语法，再跑语言自己的 linter（凭记忆，未核实源码）。代价是二进制变大，每个语法包估计几百 KB 到几 MB（未实测），建议只内置最常用的约 10 种语言。Radian 现在不依赖 tree-sitter（`Cargo.lock` 里没有）。
- **第 2 层，项目配置的检查命令（默认关，查类型和 lint）**：语言无关，Radian 只负责跑命令、截短输出。按 glob 配多条，一个仓库有多种语言时各配各的；`{files}` 换成这一批改过的文件，只查改动部分：
  ```toml
  [[edit.check]]
  glob = "**/*.rs"
  command = "cargo check -q --message-format short"

  [[edit.check]]
  glob = "**/*.rb"
  command = "bundle exec rubocop --format emacs {files}"   # 最低限度可用 ruby -wc {files}
  ```
  （配置格式是拟议，Radian 里还没有。）同一批编辑在 debounce 窗口里合并，只跑一次，把前 N 条错误（限约 2 KB）附在**这一批最后一个**编辑结果里。
- **自动探测只建议，不启用**：看到 `Gemfile` + `.rubocop.yml`、`package.json` + `tsconfig.json`、`Cargo.toml` 之类，就建议一条命令，用户确认后写进项目配置。检查命令是在用户机器上执行的代码，按 ADR 0031 不能因为仓库里有某个文件就自己跑起来。
- **为什么没有更通用的方案**：跨语言的静态检查只能到语法这一层。ast-grep（Rust，基于 tree-sitter）能写跨语言规则，但规则要有人写；Ruff（Python）、Biome、oxc（JS/TS）这些 Rust 写的检查器都只管一个生态；类型错误、未定义方法只能靠各语言自己的工具（`cargo check`、sorbet、`tsc`）。
- **不做 LSP 客户端**：LSP 要管理语言服务器进程、初始化和每种语言的差异，成本高一个量级；两层检查已经能拿到大部分收益。
- **更便宜的中间方案**：先只做通用的 `post_tool` 用户命令 hook（README 路线第 17 条，照 Claude Code `PostToolUse`），让用户自己挂 lint；第 2 层可以建立在这个 hook 之上。
- **为什么更快**：省掉“改完再调用一次 shell 检查”这一轮。Claude Code 文档说 LSP 让 Claude“不用单独跑构建”就能修错，omp 也在编辑后报诊断。
- **成本**：第 1 层低到中（依赖和二进制体积）；第 2 层中等：检查命令要走 shell 同一套沙箱和超时，还要防止它拖慢编辑工具本身（慢的检查可以异步跑、结果作为通知送回，复用 0037 的完成通知）。
- **ADR**：需要新 ADR（编辑工具结果里多出一段不属于本次编辑的内容；与 0002 的执行标记和恢复如何交互；检查结果算 durable 还是 live，见 0010）。
- **验证**：评测集上比较每个任务里编辑后 `shell cargo check` 的次数和总轮数；变异测试：关掉检查后，相应的测试要失败。

### P4. 精简定义到约 12–13 KB（收益中，成本低）
- **做什么**：
  1. schema 里的参数说明只写参数本身，不复述描述（`shell` 的 `background`/`timeout_secs`、`job_output` 的 `mode`）。
  2. `shell` 描述只留规则，删掉解释性的句子（例如“because that is where a failing command's error is”这类）。
  3. `job_list` 合进 `job_output`（不给 id 时列出全部）。
  4. `memory_revise`/`memory_save` 合成一个带 `id?` 的工具。
  5. `ask_user` 的 schema 去掉 `maxLength` 这类由宿主校验的约束。
  6. `<runtime>` 层的工具清单只在没有原生工具声明的路径上保留。

  预计删掉 6–8 KB。
- **为什么**：每轮都占上下文，也拖慢第一轮的 prefill。pi 用 2.7 KB 跑同类任务（总报告引的对比：pi 用的 token 少约 25%）。
- **成本**：低，但工具名和形状一改，dogfood 里已经学会的用法会受影响，要像 ADR 0043 那样给一个版本的别名。
- **ADR**：0043 决定 3 定了 17 个工具，合并工具要写 0043 修订；只删描述文字不需要。
- **验证**：重跑 `scratchpad/tools2/radian_specs.py` 看字节数，再在评测集上看成功率不降。这一点不能只靠直觉，因为删掉的描述里有些是 dogfood 中踩过坑才加的（如 read 的“不要加大 max_bytes”）。

### P5. `patch_ambiguous` 给出每处位置；第 4 层容忍缩进（收益中，成本低）
- **做什么**：歧义时列出前 5 处的行号和首行内容（复用 `line_of_offset`），模型能直接加上下文或改用 `replace_all`。再加一层“两端 trim 后逐行比较”的匹配，仍然要求唯一；替换时按匹配到的缩进差统一平移 `new` 的缩进，或者干脆不平移，只定位（要决定，见下）。
- **为什么**：两种失败都会多出一轮“重读 + 重试”。Codex 的 `seek_sequence` 第三档就是这样做的。
- **成本**：低。缩进平移是可选的；更保守的做法是只定位、照写 `new`，这样符合 `patch` 描述里“只定位不发明”的承诺。
- **ADR**：没有 ADR 约束 patch 的层数；描述文字要同步改。
- **验证**：给每一层写正反用例；变异测试去掉第 4 层后，相应测试要失败。

### P6. grep 加 `output: "files" | "count" | "content"`，并设总字节上限（收益中，成本低）
- **做什么**：`files` 只返回匹配文件的路径和匹配数；`content` 的总输出设上限（和 read 一样 64 KiB，或更小），超了就截断并提示。默认值要决定：Claude Code 默认 `files_with_matches`；Radian 今天的默认是 content，改成 files 会改变现有行为，建议先保持 content，只在描述里推荐。
- **为什么**：“X 在哪些文件里”是最常见的定位问题，返回行内容是浪费。总字节上限是在补一个内存和上下文的洞。
- **成本**：低。
- **验证**：测试单行很长的文件和匹配很多的仓库，确认输出有界。
- **要不要把 rg 做成内部工具（结论：不用）**：`grep` 已经是进程内工具，用的正是 ripgrep 底下的 crate：`regex`、`ignore`（gitignore 规则）、`globset`（`rad/radian-coding/Cargo.toml`）。和 rg 的主要区别是目录遍历：`search.rs` 自己写了 fd 相对、`O_NOFOLLOW` 的遍历器，替掉了 `ignore::WalkBuilder`（ADR 0039 加固）。改成调 rg 二进制或换回 rg 的 walker，会丢掉这层防软链接逃逸的保护，还多一个外部依赖。还缺的是：本条的输出模式和总字节上限、P10 的 `spawn_blocking`，以及可选的 `grep-searcher`（rg 的字面量 SIMD 加速、多行匹配）加并行遍历。最后一项只在很大的仓库上有意义，现在没有 grep 的耗时数据，要先在大仓库上跑基准确认是瓶颈，而且必须保住 fd 相对遍历。

### P7. 等待时间：自动转后台阈值从 120 s 降到 30–60 s（收益中，成本极低）
- **做什么**：`[shell] auto_background_secs` 缺省改成 60（omp 的值），用评测数据决定是否再降到 30。
- **为什么**：转后台以后模型可以继续读或改别的文件，也可以结束这一轮，等完成通知（0037 决定 3）把它叫回来。代价是有时多一轮。只有一半把握，因为 93% 的回复只调一个工具，模型未必会并行干活，要靠提示词配合。
- **ADR**：0037 修订 1 决定 1 写的是“缺省 120 s”，改缺省值需要 0037 修订。
- **验证**：同一批长构建任务比较墙钟时间和轮数。

### P8. MCP 按需加载（收益看用户，成本中）
- **做什么**：MCP 工具总定义超过阈值（例如 10 KB）时不进入请求，只在 `<runtime>` 里列出服务器名和一句话说明，再给一个 `tool_search{query}`：BM25 搜到后，把匹配工具的 schema 加到**下一次**请求。
- **为什么**：Codex、Claude Code、omp、opencode 都有。接了几个 MCP 服务器以后，定义往往比内建工具还大。
- **难点**：工具列表变化会破坏前缀缓存。Codex 把新工具加在末尾，pi 文档也提到“追加而不是改声明，以保住缓存”（`docs/research/2026-10-14-system-prompts.md` 第 86 行引用）。agent-core 的 `ToolSession::tool_specs()` 今天在会话里是固定的（ADR 0036 决定 3），所以要动 Core 的公共 API。
- **ADR**：需要新 ADR（或 0036 修订）。
- **验证**：带一个有 50 个工具的 MCP 测试服务器，看请求体大小和缓存命中。

### P9. subagent 补三个参数：`model?`、`background?`、`session?`（收益中，成本中）
- **做什么**：
  - `model`：只能选配置里允许的路由，例如 explore 用便宜模型。
  - `background`：结果走 0037 那套完成通知。
  - `session`：续接已有的子会话，子进程用 `radian exec --resume`，不用重读。
- **为什么**：Claude Code 子 agent 默认后台，可以选模型和续接；opencode 有 `sessionID` 和 `background`。用便宜模型做探索直接省钱。后台让父会话不用干等。
- **ADR**：0043 1a 第 8 条明确写着“第一版不做后台”，并要求“以后在同一个工具上加参数”。这是计划内的扩展，要写 0043 修订。
- **验证**：照 0043 1a 的验收方式补变异测试。

### P10. 文件工具放进 `spawn_blocking`（收益低，成本低）
- **做什么**：`read`/`grep`/`glob`/`list` 的同步 IO 放进 `tokio::task::spawn_blocking`，让同一批里的 Shared 调用真正并行。工具描述里“run in parallel”的说法才名副其实。
- **为什么**：现在是推断它们串行执行。按实测每次约 10 ms，收益很小，但大仓库上的 grep 会更明显。先写一个基准测试，确认是不是真的串行。
- **ADR**：不碰 ADR，只改 `radian-coding` 内部实现。
- **验证**：三个并发 grep 的墙钟时间，应约等于单次而不是三倍。

### 不建议做
- **hashline 或行号编辑**：输出 token 少，但要求每次编辑后重读拿新 tag（omp 原话：“Re-read after each edit”），而且容易因为“模型数行号很差”出错（总报告第 51 行）。Radian 的 search/replace 加“找不到给最近区域”已经是更稳的组合。
- **持久 shell**：会话状态能节省 `cd`/`source`，但会带来状态漂移和清理问题。Codex 的“可续接 PTY”只对交互式命令有用。Radian 有 `cwd` 参数和后台 job，已经覆盖主要场景。

## 未核实与限制
- Claude Code 的工具定义字节数：允许的资料里测不到。
- opencode 和 Codex 的数字是估算：opencode 少算了 enum 和约束，Codex 没算基础提示词里的 apply_patch 说明。omp 只算了描述模板原文的下限。
- “同一批 Shared 文件工具串行执行”是读源码得出的推断，没有实测。
- P2、P3、P7 的收益方向有证据，幅度要用评测集量（`6-evals-observability.md` 的区域）。
- P3 里 tree-sitter 语法包的体积、Aider 的 linter 做法、opencode 旧版的 LSP 诊断都没有核实；P6 里 grep 在大仓库上的耗时没有测过。

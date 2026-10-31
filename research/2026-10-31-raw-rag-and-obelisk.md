# 原始会话当证据：Raw-RAG 论文与 obelisk 对 radian 记忆设计的含义

> **日期**：2026-10-31
> **范围**：两份材料。arXiv 2610.30755《Is Complex Agentic Memory Always Needed? Naive RAG Is Stronger Than We Thought》（Bao、Zhang、Zhang、Lou、Wen、Luo，v1 提交 2026-10-22），读的是 `arxiv.org/html/2610.30755v1` 的正文与附录前三分之二；`github.com/tommy0103/obelisk`（AGPL-3.0）及其技能仓库 `tommy0103/obelisk-skill`（MIT）的 README，当天读取。
> **证据等级**：论文数字全部是作者自报，benchmark 全是离线对话，没有编码任务；obelisk 只读了 README，没装没跑。
> **结论**：两份材料说同一件事：提炼过的记忆比不过对原始历史的检索，记忆应当是"综合缓存"，原始证据要能被模型搜到。这支持 `docs/backlog/memory-session-search.md` 的方向，但说明它选的"只给一份清单文件、不给搜索"少了收益的来源。本文只记事实与含义，不改 backlog（owner 2026-10-31：先不要）。

## 1. 论文

### 1.1 方法

Raw-RAG 不是新系统，是一个刻意朴素的基线：「indexes source history without LLM-based extraction, summarization, graph construction, or mutation」。对话按时间戳序列化（图片换成说明文字），切成 256 token 的窗口、重叠 32 token，平铺向量索引；查询时余弦相似召回 128 个窗口，交叉编码器重排留 15 个，交给读者模型回答，读者自己「resolves temporal order and relevant state changes」。缺省设置：text-embedding-3-small 加 Cohere Rerank-v3.5，或 Qwen3-Embedding-4B 加 Qwen3-Reranker-4B；读者 GPT-4o-mini、GPT-4.1-mini、DeepSeek-V4-Flash。选中的窗口展开成完整的源轮次给读者看（MemoryAgentBench 上关掉展开）。

### 1.2 对比与结果

对比 12 个 agentic memory 系统：EverMemOS、MemOS、EMem、SimpleMem、MemU、MemMachine、Mem0、Memora、Nemori、MemoryOS、A-MEM、RecMem，加全上下文。四个 benchmark：LoCoMo、LongMemEval、MEME、MemoryAgentBench。

- Raw-RAG 在七组 benchmark×模型设置里六组第一：LoCoMo 领先最强记忆系统 0.65 到 0.95 分，LongMemEval 0.13 分，MEME（GPT-4o-mini）2.24 分，MemoryAgentBench 4.48 到 5.53 分（48.32 对 Memora 的 43.84）。
- 例外：MEME 上用 DeepSeek-V4-Flash 时 MemMachine 领先记忆系统 1.52 分，而全上下文比 Raw-RAG 高 19.98 分。
- LoCoMo 上 Raw-RAG 也高于全上下文（GPT-4o-mini：85.78 对 Nemori 报的 72.30、Mem0 报的 72.90；GPT-4.1-mini：90.87 对 80.60），但词汇指标 F1 全上下文更高。
- 记忆系统赢的地方（子任务）：LoCoMo 开放域问题 Raw-RAG 55.90，Zep 76.60、Mem0g 75.71、Mem0 72.93；LongMemEval 的知识更新与时间推理 Memora 97.40 与 89.50 对 Raw-RAG 91.03 与 81.45；MEME 的聚合、删除、级联题 Raw-RAG 只有 19.00、26.00、4.27；PersonaMem-v2 上 Raw-RAG 不领先任何场景，总排名第三。

### 1.3 为什么

作者把准确率拆成覆盖（召回到足够证据的比例）和利用（证据够时答对的比例）。MemoryAgentBench 上 Raw-RAG 覆盖 45.29%，各系统范围 19.07% 到 45.29%；利用 84.43%，范围 80.00% 到 89.71%。原话：「Raw-RAG leads in coverage, helping offset its less-than-best conditional utilization」，并把「coverage as the broader cross-system bottleneck」。

更有分量的一组：给每个系统的记忆库开"上帝视角"检索（complete-store oracle），七个系统都涨，Raw-RAG 到 57.54%、MemMachine 57.46%，但原始证据对照是 61.60%；不设打包上限时七个记忆库仍比原始证据低 7.58 到 29.38 分。作者自己加了注：「the residual gap is not a pure measure of information loss during ingestion」，证据拼装和 lost-in-the-middle 也有份。

Mem0 的增删改审计：把 UPDATE 当 ADD、DELETE 当空操作（ADD/NONE-only）后 LoCoMo 从 67.36% 涨到 69.42%。

### 1.4 成本

MemoryAgentBench 11 个非冲突源的建库开销（表 32）：

| 配置 | LLM 调用 | token（M） | 总时间（s） |
|---|---|---|---|
| Atomic facts | 40,248 | 46.3 | 31,673.8 |
| Raw summary | 32,144 | 21.6 | 10,327.5 |
| Graph-expanded | 33,748 | 109.3 | 15,599.6 |
| Raw-RAG | 0 | 0 | 718.4 |

Raw-RAG 建库和检索不打一次模型。

### 1.5 作者承认的限制

「Our evidence is limited to offline conversation on public benchmarks」，不能推广到「interactive, multimodal, or broader operational settings」；没测弱读者；原始历史「raise storage and privacy concerns」；top-k 从 15 提到 60 只在长程理解上有 2.22 分、总分经 Holm 校正不显著；「one global context size need not be optimal for every capability」。

## 2. obelisk

- 一句话（仓库描述）：「Every past session, subagent, and workflow -- queryable by your agent, browsable by you」。
- 做什么：「Obelisk indexes your local coding-agent history into one SQLite database and serves it to two audiences」：给 agent 的 CLI 加 skill，给人的 Electron 桌面 app（浏览会话、管记忆、用量、周报卡片）。
- 存储：`~/.obelisk/obelisk.sqlite`，FTS5 全文索引，监视各 agent 的目录增量重建；app 运行时拥有写入，CLI 只读。
- 查询方式：agent 写一段 JS 查询，`obelisk --query <script>` 执行，读 JSON 结果；助手函数 `overview()`、`sessions()`、`search()`、`messages()`、`memories()`、`summaries()`、`sql()`。
- 记忆：「When a retrieval produces a conclusion worth keeping, the agent proposes a markdown memory file」，「After your approval, it registers the file with `obelisk --attune <script>`」，「a synthesis cache, not a replacement for raw evidence」。记忆文件的位置与格式 README 没写。
- 安全一句：「Transcript content is attacker-controlled」。
- 支持的 agent 与路径：Claude Code `~/.claude/projects`、Codex `~/.codex/sessions`、Copilot、DeepSeek Harness、Hermes、Kiro、Kimi Code、OMP `~/.omp/agent/sessions`、OpenCode `~/.local/share/opencode/opencode.db`、Pi `~/.pi/agent/sessions`、ZCode。**没有 radian。**
- 其他：760 星、223 次提交；Node 22.13 以上，CLI 零运行时 npm 依赖；没写它自己是否调用模型、没写遥测。

## 3. 对 radian 的含义

radian 这周定的记忆设计（`docs/backlog/memory-files.md`、`memory-session-search.md`，PL0078）是：模型可写的 Markdown 记忆文件加索引注入；过去的会话用一个宿主维护的只读 `SESSIONS.md`（每会话一行：id、时间、首条用户消息）让模型 `read`，不给搜索工具。

1. **记忆文件的形态对了。** 两份材料都把提炼出来的记忆当"综合缓存"，原始证据另算。论文给了数字：提炼库就算检索完美也比原始证据低 7.6 到 29 分；增删改不帮忙。我们已经删掉的 revision、KeyConflict、purge 没有必要再回来。
2. **会话索引少了收益的来源。** 论文的收益全在覆盖率，覆盖率来自对原始文本的检索。`SESSIONS.md` 只有首句，本机 1186 个会话，模型靠首句找"上次改 compaction 那次"基本靠运气；它能当入口，当不了检索。obelisk 的做法是 FTS5 加一个 agent 可调用的查询面。若要拿到论文那种收益，下一步是给模型一个对已提交 journal 的全文搜索（TUI 已有人用的全文搜索可复用），结果按低可信数据处理、排除当前会话。这一步 owner 2026-10-31 说先不做。
3. **原始证据也是攻击面。** obelisk 的「Transcript content is attacker-controlled」和 radian TK0276 的输入防护是同一个判断：会话检索结果与 MCP 结果一样，是数据不是指令，而且读过它之后能不能写记忆要按同一条规则。
4. **全上下文装得下时仍是上策。** DeepSeek-V4-Flash 上全上下文比 Raw-RAG 高 20 分，与 radian 默认大窗口、把压缩当成本的立场一致（见 `docs/backlog/compaction-threshold.md` 的数据）。
5. **成本上 Raw-RAG 是零模型调用。** 离线提炼（Codex、oh-my-pi 的形状，`memory-provider.md`）每千个会话要几万次调用；全文索引不要。这是把 `memory-provider.md` 的 capture 继续留在 idea 的又一个理由。
6. **子任务例外要记着。** 聚合、级联、知识更新这类题记忆系统赢，也就是"把多条证据合成一个结论"的场景。radian 里对应的是复盘（retro）而不是检索，合成的产物进 worker-brief.md 或记忆文件，仍然是人把关的综合缓存。

## 4. 没核实的

论文附录最后三分之一（约 32k 字符）没读；Mem0 增删改审计的比率图（图 2）没看到；全上下文在 LoCoMo 以外的数字论文没给；obelisk 的记忆文件格式、是否调用模型、遥测；obelisk 在本机的实际表现（没装）。

## 5. 来源

- https://arxiv.org/abs/2610.30755 与 https://arxiv.org/html/2610.30755v1（2026-10-31 读）
- https://github.com/tommy0103/obelisk 、https://github.com/tommy0103/obelisk/blob/main/README.md 、https://raw.githubusercontent.com/tommy0103/obelisk/main/SKILL.md（安装引导，不是查询技能）、https://github.com/tommy0103/obelisk-skill（2026-10-31 读）
- 对照：`research/2026-10-31-competitor-memory.md`

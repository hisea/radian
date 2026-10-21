# Radian 分区域深挖：总报告

2026-10-18 · 对比对象：pi、Codex CLI、Claude Code、omp、opencode · 代码版本 hisea/radian@356df1e

六份分报告：
1. [Core loop](1-core-loop.md)
2. [工具层](2-tools.md)
3. [Context](3-context.md)
4. [架构](4-architecture.md)
5. [模型与 provider 层](5-provider-layer.md)
6. [评测与可观测性](6-evals-observability.md)
7. [权限与沙箱的交互摩擦](7-permissions-sandbox.md)
8. [扩展机制](8-extensibility.md)
9. [长任务与记忆](9-long-tasks-memory.md)

第三轮（速度和效率之外的方向）：

10. [产品定位与差异化](10-positioning.md)
11. [生态兼容](11-ecosystem-compat.md)
12. [接入场景](12-integration-surfaces.md)
13. [安全加固](13-security.md)
14. [开发流程与工程健康](14-dev-process.md)
15. [上手体验与分发](15-onboarding-distribution.md)
16. [跨平台](16-cross-platform.md)

每条结论在分报告里都有出处（源码用 `文件:行号`，其他给链接）。Claude Code 是闭源的，相关结论全部来自官方文档。标了“推断”的还没实测。

## 一句话

**Radian 的骨架已经很快**：单个 Rust 二进制，启动约 98 ms，一轮 harness 开销 17 ms，系统提示词的前缀稳定性在六家里最好。**慢和浪费主要出在骨架之外**：长工具等待、缓存寿命太短、摘要请求读不到缓存、工具定义偏重、开了沙箱 shell 仍然每次都要问，以及循环不会从“无效结束”里自己恢复。另外，Radian 目前完全没有评测，这些问题的收益都量不出来。

## 各区域的优势和弱势

| 区域 | Radian 的优势 | Radian 的弱势（关键证据） |
|---|---|---|
| Core loop | 每轮固定开销低（组提交后 17 ms，工具往返 26 ms）；连续的只读工具已经并行 | 空回复直接结束这一轮（`turn.rs:1817`）；输出截断且没有工具调用时也直接结束（`turn.rs:1829`）；没有重复调用检测（omp 连续 5 次相同就纠正）；长工具运行时 steer 进不来，最长要等 600 s |
| 工具 | 报错一次说清怎么改；`patch` 有三层宽松匹配；输出都有上限 | 工具定义最重：17 个共 20 KB，约 5k token，pi 只有 2.7 KB；`patch` 不能一次改多处；shell 截断的中间部分直接丢掉，只能重跑；没有编辑后诊断；MCP 不能按需加载 |
| Context | 前缀稳定性最好（0a–3 层一个进程只算一次）；AGENTS.md 有上限并标为不可信 | 只用 5 分钟的缓存 TTL，而 `job_output` 中位等待 361 s，等完后缓存大概率已经过期（推断）；摘要请求换了一套 system，整段历史按全价计费（`compaction.rs:356-385`）；没有“只清旧工具输出”这一档；压缩后不补读最近的文件，模型要重查 |
| 架构 | 单进程、不需要 daemon，可嵌入；产品代码约 9.3 万行，和 pi 同一量级，远小于 omp 和 Codex | **开了沙箱 shell 仍然每次都要问**（`radian-permissions/src/profile.rs:139-170`，workspace 和 full 两档都是 `ask`），所以 dogfood 只能用 `full + --yes`；非 auto 模式下 `general` subagent 实际跑不了 shell；`radian exec` 没有机器可读的输出；没有用户 hook；43 个 ADR 和 6–7 分钟的 Verify 拖慢迭代 |
| Provider | 三种协议都有 adapter，有契约测试；Anthropic 自动打缓存断点 | ChatGPT 路由每次调用都新建 `reqwest::Client`，连接不能复用（`radian-host/src/model.rs:240-251`，已核对）；OpenAI 路由不发 `prompt_cache_key`；没有把杂活分给小模型；没有 Responses 的增量请求 |
| 评测 | journal 已经记录了时间戳、工具起止、每次调用的 usage 和费用，指标可以直接从里面算 | 没有任务集、没有指标提取器、没有和 pi 的配对对比；首 token 时间不进 journal；看不到缓存命中率 |
| 权限与沙箱 | 沙箱后端拒绝在没验证过的环境里运行；权限层和执行层分开 | 开了沙箱，shell 仍然每次都问（shell 占 dogfood 工具调用的 53%）；复合命令永远不能 allow，授权按完整命令串记，会话结束就失效；无头运行、后台通知、herdr 发起的回合以及 subagent，碰到 ask 只能被拒。**前置安全缺口**：Linux bwrap 的 workspace 档能连 Unix socket（包括 `docker.sock`），做免审批之前必须先用 seccomp 堵上 |
| 扩展 | `Hooks` trait 已经能拒绝、改参数、直接给结果、改写结果，出错一律 fail closed，能力相当于 Claude Code 的 Pre/PostToolUse | 没有用户层的命令 hook；项目级 skill 已经由 ADR 定下但没接上（`skills/mod.rs:106-133`）；MCP 只能用命令行参数配置，按顺序启动，一个失败整个会话打不开，结果也不截断；**疑似问题**：`explore` subagent 会拿到父会话的 MCP 配置，而 MCP 工具不受只读白名单限制（`subagent.rs:390`、`tools.rs:242-254`，从代码推断，未运行验证） |
| 长任务与记忆 | 中断恢复最严谨（区分结果未知和没开始的工具调用，提醒孤儿后台 job）；记忆的治理是六家里最严的 | dogfood 里多次出现“只查不改”：D24 一轮 445 次调用、10 次压缩，没有一次编辑；没有 todo 或 `/goal` 这类持久目标，也没有轮末自动续跑；resume 后要人手动说“继续”；`memory_save` 默认 ask，也不说明什么时候该存 |

## 合并后的改进路线（按收益/成本排序）

**第 0 步，先让收益看得见（不改 Rust，不碰 ADR）**
1. 写一个 journal 指标提取器，算成功率、耗时、token、缓存命中率、工具调用数和单次成功成本，再搭一个最小评测集：15 道 Terminal-Bench 2.0 子集题加 5 道从 dogfood 提炼的真实任务，Radian 和 pi 各跑 3 次。设计见分报告 6 的 §7。

**第 1 批：改动小，收益直接**
2. ChatGPT 路由复用 HTTP client（只改一处，省掉每步重新握手）。
3. OpenAI 和 Responses 路由发 `prompt_cache_key`；Anthropic 补上第 4 个缓存断点。
4. 缓存 TTL 改用 1 小时，或者在长工具运行期间给缓存保温（pi 默认开启）。
5. `patch` 支持一次改多处（`edits[]`），匹配失败时列出候选位置的行号，再加一层容忍缩进偏差的匹配。
6. shell 截断时把全文写进文件并给出路径，同时下调默认输出上限。
7. 循环层：空回复自动重试（最多 3 次）；输出被截断时提示模型拆小继续；重复调用检测可以做成宿主 hook，不用改 agent-core。

**第 2 批：需要修订或新写 ADR，收益大**
8. **沙箱内自动放行 shell，越界时再问**（修订 ADR 0012），subagent 也继承这个规则。这一条同时改善速度和交互摩擦，是架构上收益最大的一项。
9. 摘要请求复用原对话的 system、工具和历史，以便读到缓存（修订 ADR 0018，需要和“摘要降档”一起权衡）。
10. 在整段摘要之前加一档“清旧工具输出”，压缩后补读最近改过的文件（修订 ADR 0018 和 0019）。
11. 长工具更早转后台（120 s 降到约 60 s），运行中允许 steer 打断（修订 ADR 0011 和 0037）。
12. 精简工具定义：合并或按需挂载低频工具，去掉 schema 里和描述重复的文字，`<runtime>` 层不再重复列工具（修订 ADR 0036）。
13. `radian exec --output jsonl` 加结果摘要（修订 ADR 0032），给评测和 CI 用。
14. subagent 可以单独指定模型，压缩也可以用小模型。

**第 3 批：权限、扩展、长任务（来自分报告 7–9）**
15. **“迟迟不编辑”看门狗**：长时间只读不改时，宿主自动发一条 steer。dogfood 里唯一管用的就是 owner 手动发的这条 steer。不改 agent-core，复用 inject 通道，需要新 ADR。先在 D26 的任务上做 A/B。
16. **沙箱内免审批的完整方案**：先堵 Linux 的 Unix socket（seccomp），再让 workspace 档沙箱里的 shell 自动放行。沙箱拒绝时，模型显式请求越界才问人，绝不自动去沙箱外重跑。复合命令逐段判断；授权改成按前缀，加禁止清单，按项目持久化。subagent 继承父会话的放行规则。（扩展第 8 条，修订 ADR 0012、0016、0043）
17. **两个外部命令 hook**：`post_tool`（编辑后跑 lint 一类）和 `stop`（轮末验证，不通过就自动开下一轮，最多 3 次）。stdin 用 Claude Code 的字段名，方便复用现成脚本。进程开销实测 3–24 ms。
17b. **编辑后诊断分两层**（2-tools P3，2026-10-18 讨论后补充）：不做成新 tool，由宿主在编辑成功后自动跑。第 1 层内置 tree-sitter 语法检查，零配置、默认开、不执行仓库代码；第 2 层是按 glob 配置的项目检查命令（`cargo check`、`rubocop {files}` 等），默认关，自动探测只建议不启用（ADR 0031）。不做 LSP 客户端。
18. **接上项目级 skill，并支持 `$ARGUMENTS`**：约 100 行，ADR 已经定过，可以顶替被删掉的提示模板。
19. **持久化的 todo 和 `/goal`**：确定性地带过压缩，轮末根据目标自动续跑，带预算和完成判定。resume 后一键续跑。
20. **核实并修正 `explore` subagent 的 MCP 问题**；MCP 改成并发启动，失败不阻塞会话，结果加截断。
21. 记忆写入指引，以及按项目放行 `memory_save`；`memory_save`/`memory_revise` 合并，`memory_search` 按需挂载（实测 479 次工具调用里 memory 工具 0 次，见 9-long-tasks-memory §6.5）。

**先不做，等评测数据说话**
- 边流边派发工具：93% 的回复只有一个工具调用，估计省不了多少。先记录首 token 时间和工具就绪的时刻。
- Responses WebSocket 增量请求：成本最高，等数据证明上传和 prefill 是瓶颈再做。
- 改成 client/server 或 daemon、主存储换 SQLite、树状分支、进程内 subagent、继续拆 crate：都不值得。
- 把 rg 做成内部工具：`grep` 已经在进程内用 rg 的同一组 crate，换成 rg 二进制会丢掉 ADR 0039 的 fd 相对遍历。只补输出模式和总字节上限（2-tools P6）；`grep-searcher` 和并行遍历等大仓库基准证明是瓶颈再做。

## 不建议做的（来自分报告 7–9）

- 用分类器判断的 auto 模式、让模型提议持久授权规则、沙箱拒绝后自动在外面重跑、缺省全放行。
- 大而全的插件系统或插件市场。想吸纳生态，兼容 Claude Code 的格式最便宜（Codex 和 omp 已经这么做了）。
- 文件回滚（undo）先排后面：各家做法分歧很大，Codex 已经删掉了自己的 undo。

---

# 第三轮：速度和效率之外

## 一句话

**Radian 的工程质量和结构性安全在六家里最强，但还没有走到用户面前**：仓库是私有的，没有 CI，没发过版，也没有安装方式；首次启动按正常顺序走会报错退出两次；只能在终端里用，编辑器、CI 和 SDK 都接不上。下一阶段的重心应该从“把内核做对”转到“让别人能用上”。

## 各区域要点

| 区域 | 优势 | 主要缺口 |
|---|---|---|
| 定位 | 单个二进制（启动 98 ms，空会话 11 MB）；内核可以直接链进 Rust 进程；journal 可回放校验；默认不出网有测试证明 | 0 个用户，没发布过。社区规模参考（2026-10-18）：opencode 21.5 万星、Claude Code 15.2 万、Codex 12.8 万、pi 11.6 万、omp 3.4 万。pi 10 月推出实验性的 Pi Durable，正在追“可恢复”这个卖点 |
| 生态兼容 | 完整读取 AGENTS.md；已经有一次性导入别家格式的先例（`import-pi`、主题导入） | 不读 `.agents/skills`；MCP 用自己的配置格式，现成的 `.mcp.json` 一个字都用不上；没有 hook。业界正在收敛到四个中立标准：AGENTS.md、Agent Skills、MCP 的 `mcpServers` JSON、Agent Plugins |
| 接入场景 | 分层很适合加前端：审批、提问、事件都能注入，不用改 agent-core | 不在 ACP 里（已有 13 个编辑器客户端、47 个 agent 接入）；无头模式没有 JSON 输出，五种结局（未完成、拒答、超预算、取消、失败）的退出码都是 1；`-p` 不能从 stdin 读 |
| 安全 | 结构性安全最强：子进程环境变量白名单（只传 16 个名字）、信任门 fail closed、文件工具全程 fd 相对加 `O_NOFOLLOW`、宿主 git 不执行仓库配置里的命令。对手公开过的 8 个 CVE 里，有 6 个所属的类别 Radian 在设计上已经堵住 | **高**：缺省 `full` 档下 MCP 调用直接放行，MCP 进程也不进沙箱，一次提示注入就能外泄秘密（`profile.rs` 已核对）。**中高**：工具输出里的秘密不脱敏，原样发给 provider、写进 journal。**中**：没有 SECURITY.md，不扫描 410 个依赖的安全公告。另有两处小缺口：宿主上的 `git diff` 没有禁用外部 diff 和 textconv（已核对 argv，危害程度是推断）；沙箱探针用可预测的 `/tmp` 路径 |
| 开发流程 | 质量纪律最强：43 个 ADR、约 1495 条变异测试记录、契约测试、journal 自检 | 没有 CI，没有分层验证。6 天里 2190 个提交，按每天约 88 次合并推算，全量 Verify 每天要约 9 小时机器时间；测试 target 3 天内从 162 个涨到 200 个；ADR 重写积压（0018 有 60 KB、9 次修订）；AGENTS.md 有 11.6 KB，每个会话都加载 |
| 上手与分发 | 没有运行时依赖；release 包优化后 24.6 MiB（gzip 后 10.7 MiB），Codex 要 95–110 MB；ChatGPT 登录已经做好 | 新用户装不上（只能从源码编译，约 4 分钟）；首次启动报错退出两次，报错提示还指向一条走不通的路；`--help` 里夹着 ADR 编号，provider 说明也过时了；没有 doctor、更新和卸载说明。从零到第一次编辑：Radian 7–8 步，对手 3–5 步 |
| 跨平台 | agent-core、runtime、权限和 provider 层没有任何平台专用代码 | 六家里唯一没有 CI、也不发二进制的。Windows 上超时时子进程不会被杀，shell 写死 `/bin/sh`，`radian-host` 很可能编译不过（推断） |

## 推荐的定位

**“可以放心交出去、无人值守跑的 worker agent”**：在 herdr、CI 和其他产品里放手让它干活，靠沙箱、可回放的 journal 和严格的权限让人放心。这个方向和 ADR 0043、0034 已经定下的方向一致，不用推翻任何 ADR，也避开了在 pi 主场（极简加庞大生态）正面竞争。“可嵌入的 Rust 内核”先当作支撑这个故事的证据，等 API 稳定后再发布 crate。不建议定位成“更严谨的 pi”。

支撑这个定位最关键的三个缺口，和前两轮的结论是重合的：沙箱内免审批（第 16 条）、`radian exec --output jsonl`（第 13 条），以及公开发布和分发。

## 第三轮的改进路线（按对整体价值的收益/成本排序）

**马上能做，不碰 ADR**
22. 修正 `--help` 和 README 里过时的文字，去掉 ADR 编号和内部术语。
23. 安全小修：宿主 `git diff` 加 `--no-ext-diff --no-textconv`，沙箱探针改用不可预测的临时路径。
24. 在 README 写明 WSL2 按 Linux 对待，并在 WSL2 上实测沙箱。
25. 开发流程：分层验证脚本（fast / affected / full，full 的定义不变）、pre-commit 跑 fmt 和 clippy、`perf_*` 测试加 `required-features`、ADR 索引补全、AGENTS.md 瘦身（把委托 pi 的规则挪进 skill）。

**需要 决定或新写 ADR**
26. **做完首次启动流程**：ADR 0008 补记里定的事（首次生成默认配置、没有模型时打开 `/model` 选择器）代码还没做，同时允许没有凭据也进 TUI，在里面 `/login`。做完后能降到 3 步。
27. **CI 和发布**：在 main 上跑 Linux 和 macOS，覆盖 musl 和 arm64，Windows 只做编译检查；加 `cargo-deny`；发布产物，提供 `install.sh` 并配置 `[profile.release]`。前提是 决定公开仓库，还需要一份分发 ADR。
28. **SECURITY.md 加一页威胁模型**，附上 CVE 对照表，把结构性安全做成对外卖点。
29. **`full` 档下 MCP 改成按 server 首次询问**，并对工具定义做哈希固定（修订 ADR 0012，涉及 0043 决定 2）。
30. **工具输出进 journal 和 provider 之前做已知秘密脱敏**（修订 ADR 0013 T4，并保证 journal 自检重放一致）。
31. **生态兼容**：读 `.agents/skills`；MCP 接受 `mcpServers` 格式（先只做 stdio，项目级的 `.mcp.json` 走信任门）；hook 用和 Codex 同构的子集；`radian import claude` 一次性导入 Claude 的私有格式。
32. **入口**：先做 `radian exec --output jsonl`（附结果行、细分退出码、`-p -` 从 stdin 读），约 2–3 天；再做控制口客户端，让 herdr 改走它，约 2–4 天；最后做 `radian acp`，接入 13 个编辑器，约 1.5–2.5 周。
33. `radian doctor --json`；`radian login --with-api-key` 从 stdin 读 key，给 CI 用。

**先不做**
- 原生 Windows（约 4–6 人周，之后还要持续修 bug），等有外部用户再说；原生 Windows 沙箱不做。
- 自建 app-server、远程和手机端、Slack、各语言 SDK 包。
- 运行时读整个 `.claude/` 目录、完整兼容 Claude 插件和 marketplace、导入别家会话。
- 合并测试二进制收益很大，但风险已经测到（14 个文件调用 `set_var`，41 个文件有全局状态），需要单独写 ADR 慎重处理。

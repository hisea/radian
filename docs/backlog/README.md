# 待办（backlog）

状态：accepted

## 规则

- ADR 是**代码现在样子的快照**。打算做、还没做的事放在这里，一件事一个文件。
- **引用只走一个方向**：backlog 文件可以引用 ADR（`涉及 ADR`），ADR 绝不引用 `docs/backlog/`。事项做完就删掉对应的 backlog 文件，ADR 里永远是干净的快照。
- backlog 条目**不是 worker brief**：只写稳定的东西（是什么、为什么、定好的设计、涉及哪些 ADR、怎么算做完）。worktree 路径、在某提交核对过的行号、命令，写进派发时现写的 brief，不写在这里。
- 事项落地时，同一个分支做两件事：**（a）** 在自己的 docs-only commit 里改 ADR，**（b）** 删掉 backlog 文件；两件事和代码一起合进 `main`。`main` 上的 ADR 永远和代码一致。
- 状态只有三种：`idea`（没定）、`decided`（设计已定，可以派发）、`in-progress`（已派发，注明分支）。

## 条目模板

```md
# 标题

- 状态：idea | decided | in-progress（分支名）
- 涉及 ADR：0008、0026
- 来源：研究报告路径与章节

## 背景

## 决定（已定）/ 待决问题（未定）

## 验收
```

`idea` 的条目可以只有 5–15 行；不要编造决定，开放问题就写成开放问题。

## 索引（按状态排：in-progress、decided、idea）

| 文件 | 状态 | ADR | 一句话 |
|---|---|---|---|
| `turn-end-continuation.md` | A item 4 open；B 已落地 | 0036、0037、0008、0045、0011、0021、0034、0005、0017 | 轮末「只说不做」（`Now X.` 后无工具调用）：A 部分落地（删掉无效的结束回合规则、工作区外可见加任务文本例外、通知以 omp 形状收尾并修少换行、显式转后台返回文本按 opencode 形状、取消自动转后台即后台只走显式 `background: true` 且前台缺省 300 s），只剩查 lead 记忆漏进 worker（第 4 项）；B `Stop` 路径内建续跑已落地（保守规则粗筛 + 一条 System 续跑交模型判断，每回合一次、连续两次后 herdr `blocked`，计入唤醒闸门，stdin 加 `last_assistant_message`）；研究笔记 `research/2026-10-21-intent-stop.md` |
| `build-cache-and-verify-time.md` | idea（3、4；1、2 已落地） | 无；第 4 项新 ADR | Linux worker 依赖缓存（sccache / ZFS block cloning）与合并测试二进制为 idea |
| `arc-landing.md` | decided | 0048 | 合并方式可配置（`push` 三种 style、`pr`、`command`，arc 只保证验过的内容落地）与钩子（`after_merge`/`after_fail`/`after_rework` 跑配置里的命令，失败只警告）；默认钩子删 worktree 与 `work/<task>` 分支 |
| `tool-source-tag.md` | idea | 0012、0043 | 工具来源注册时打标签，替代 `__` 推断 MCP；与 mcp-servers-json 一起（外部审核 P2-4） |
| `trust-load-in-session.md` | idea | 0031、0004、0025、0008 | TUI 弹窗信任后当场装载项目 skill 与 `.mcp.json`（今天要下次启动；exec 当场装载） |
| `overflow-three-stage.md` | idea | 0018、0017 | 超长兜底按 70% → 50% → 35% 三档推进，今天只有一档 50% |
| `memory-provider.md` | idea | 0021、0013、0031 | MemoryBackend trait + 命令 backend + 离线 capture |
| `subagent-ask-forwarding.md` | idea | 0043、0010、0024 | 子会话仍要问的 ask 转回父会话；缺 durable `PermissionRequested` 与 control 收审批；依赖沙箱内放行 |
| `verify-test-binaries.md` | idea | 新 ADR | 合并集成测试二进制压 Verify 时间；`set_var` 与全局状态先处理；release 调优不做 |
| `mcp-full-tier-ask.md` | idea | 0012、0043 | full 档 MCP 按 server 首次询问，工具定义按哈希固定 |
| `eval-metrics.md` | idea | 无 | journal 指标提取器 + 固定任务集；Terminal-Bench 适配器在 Linux 盒子上做 |
| `cache-efficiency.md` | idea | 0018、0026 | `prompt_cache_key`、TTL、摘要复用前缀；实测现用路由命中 98.7%，接入 Anthropic 路由时再做 |
| `context-pruning.md` | idea | 新 ADR | 轻量档：清被同路径后续 `read` 取代的旧结果（占工具字节 24%）；条件：阈值 A/B 收益不够时才做 |
| `compaction-threshold.md` | idea | 0018、0008 | `[compaction] threshold` + 压缩后确定性补水；条件：eval-metrics 落地后做，A/B 定缺省值 |
| `arc-flows.md` | decided（1、2）；idea（3、4） | 0048、0043、0044、0025、0008 | 一个仓库按任务种类选 worker 参数与 gate profile：`[flows.<name>]` + task `flow` 字段；先落地 `review` stage；非代码资产的交付规则待定；第 4 步 trigger（clock / every N merges）与 ratchet / retro / fix 三个 flow 为 idea |
| `herdr-prompt-radian.md` | decided（跟踪上游 #4732） | 0034 | herdr 的 `agent prompt`/`send-keys` 只认编译进去的 agent 种类，自报的 radian 被拒；不提 PR（herdr 只收名单内 PR），在 herdr #4732 补复现；我们用 `pane send-text` + `send-keys enter` |
| `prompt-deliver-first.md` | idea | 0036 | 提示词：先交付能跑的版本再迭代、用参照物差分；cobol smoke 是现成复现 |
| `runtime-resource-limits.md` | idea | 0036 | `<runtime>` 写实际可用 CPU/内存（cgroup），避免 `make -j16` 跑在 2 核容器 |
| `deadline-awareness.md` | idea | 0017、0036、0032 | radian 知道回合期限、自己收尾，并（待定）告诉模型剩余时间 |
| `arc.md` | in-progress（切片 1–7 已落地） | 0032、0043、0044、0038、0020、0024、0013、0037、0022 | 替代 herdr 的派发与闸门：文件 backlog 单写者、一次性 worker、固定 sha 的 gate；产品形态为三界面 app 加 plan 层与全自动 |
| `arc-gate-batch.md` | decided（排在 `arc-landing.md` 之后） | 0048 | gate 合批：单个 task 只做快速检查，可合并的 task 叠在 main 上只跑一次完整 Verify，失败再拆开定位 |
| `arc-reviewer-profiles.md` | decided | 0048 | reviewer profile（模型、prompt 写审查范围、时限、`fix_level`，第一轮可另设模型与时限）；只审 diff，返工轮只审增量和上轮问题，`pre_existing` 由 arc 按改动行机械判定、不打回；问题分 `critical`/`major`/`minor` 并附校准例子，用户选修到哪级，task 可覆盖；review 与 commands 并行，review 失败由 arc 自动写返工；没修的问题变成 `todo.proposed` 收尾 task，新增 task 级 `confirm` |
| `arc-serve.md` | decided（钩子排在 `arc-landing.md` 之后） | 0048、0013、0020、0008、0034 | 远程客户端的服务端：一个 hub 管本机登记的所有仓库，不持 `runner.lock`，owner 动作走 CLI 库函数与 `state.lock`；只听 loopback，token 必须；`/v1/` 读接口与 `list`/`show --json` 同构，加报告、verdict、diff；SSE 事件只是提示；通知走 `[hooks]` 加 `after_awaiting_human`/`after_proposed`，radian 不发推送；v1 不看实时会话 |
| `arc-ios.md` | idea | 0048、0034 | 原生 SwiftUI 客户端，只连 hub；扫码配对、Keychain 存 token；v1 列表、详情、diff 与八个 owner 动作，不看实时会话；锁屏通知走 Bark/ntfy 钩子跳 app 的 URL scheme；待决：仓库位置、分发、最低版本 |
| `session-arc-page.md` | idea | 0048、0023、0027、0035 | 会话 TUI 里 `/arc` 切到 arc 的 plans/task/merge 页面，不退出会话；读当前项目的 `.radian/arc/`，审批弹窗仍在最上层，owner 动作与 `radian arc` 相同，模型一侧调不到 |
| `acp-server.md` | idea（第二阶段：编辑器；第一阶段已落地，见 0049） | 0049、0024、0031、0023 | 只剩编辑器场景：多 session 是一个连接多会话还是每线程一个进程、编辑器里的信任门怎么问、要不要调客户端 `fs/*` 看未保存缓冲区、上 agentclientprotocol.com 的列表；第一阶段的用户是 arc（arc 即 ACP 客户端已落地） |
| `hooks-stop-and-feedback.md` | idea | 0045、0037、0017、0012、0046 | `Stop` stdin 加 `stop_hook_active`/`background_tasks`/`session_crons`；`additionalContext` 非错误反馈与 `PostToolUse` `updatedToolOutput`；续跑计数在工具调用后清零、滑窗留作硬顶；`args` exec 形式与每次运行一行耗时 |
| `hooks-async.md` | idea | 0045、0037、0011、0046 | `async: true` 的 hook 接到后台 job 上：exit 0 的 `additionalContext` 走 `inject`，`asyncRewake` 退出 2 走 `followup` 计一次 wake；post-edit 测试不再拖慢回合 |
| `hooks-permission-request.md` | idea | 0045、0012、0043、0048、0024、0011 | `PermissionRequest` 事件让脚本回答 ask（只回答 ask，不扩权）；headless worker 与 arc 的审批出口，`addRules` 到 `session` 记住选择；与 `subagent-ask-forwarding.md`、`arc-serve.md` 第 4 条同一需求 |
| `hooks-lifecycle-events.md` | idea | 0045、0011、0036、0018、0021、0034、0017、0013 | 五个生命周期事件按用例逐个加：`UserPromptSubmit`、`SessionStart`、`Notification`（只两个 matcher）、`PreCompact`/`PostCompact`、`StopFailure`；研究笔记 `research/2026-10-24-claude-code-hooks.md` 含不采纳项 |
| `provider-gemini-native.md` | idea（方向已定：做） | 0026、0014、0007、0015、0017 | Gemini 原生 `generateContent` adapter，过 provider contract；第二步 Vertex（同协议加 GCP 认证） |
| `provider-azure-responses.md` | idea | 0026 | Azure OpenAI 走 Responses：同一 adapter 的配置变体（deployment URL、`api-version`、`api-key` header），有用户需要再做 |
| `provider-bedrock-converse.md` | idea | 0026、0020、0013 | Bedrock Converse：SigV4 签名加 AWS 二进制事件流，没有明确需求不做；先看 OpenAI 兼容接口能否替代 |
| `provider-compat-flags.md` | idea | 0026、0014、0008 | 照 pi 的 `compat` 按真实故障逐个加模型兼容开关，不一次照搬；首个候选是会话粘性 header |
| `provider-mistral-and-pi-messages.md` | idea（不做） | 0026 | Mistral 已有 OpenAI 兼容 chat；pi-messages 是 pi 自家网关私有协议；写明重新考虑的条件 |

只占索引行（暂无文件；来源都是总报告 `research/2026-10-18-area-deep-dive/README.md`）：

| (无文件) | 状态 | ADR | 一句话 |
|---|---|---|---|
| no-edit 看门狗 | idea | 新 ADR（第 15 条） | 长时间只读不改时宿主自动发 steer；先在 D26 任务上 A/B |
| 持久 todo + `/goal` | idea | 新 ADR（第 19 条） | 目标确定性带过压缩，轮末自动续跑，resume 后一键续跑 |
| CI 与发布 | idea | 分发 ADR（第 27 条） | Linux/macOS CI、`cargo-deny`、安装方式；前提是公开仓库 |
| SECURITY.md | idea | 无（第 28 条） | 一页威胁模型 + CVE 对照表 |
